//! Optional local-only embeddings. Rust owns chunking, provenance, cache and ranking;
//! native ONNX inference runs in a bounded, separately killable CLI worker.
use crate::ManagedDocument;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn timeout() -> u64 {
    30_000
}
fn threads() -> usize {
    2
}
fn batch() -> usize {
    8
}
fn chars() -> usize {
    1000
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConfig {
    pub model_dir: PathBuf,
    pub runtime_library: PathBuf,
    #[serde(default)]
    pub worker: Option<PathBuf>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "threads")]
    pub threads: usize,
    #[serde(default = "batch")]
    pub batch_size: usize,
    #[serde(default = "chars")]
    pub chunk_chars: usize,
}
impl SemanticConfig {
    pub fn from_env() -> Result<Self, String> {
        let p = std::env::var_os("MNEMOSYNE_EMBEDDING_CONFIG")
            .ok_or("semantic_unavailable: set MNEMOSYNE_EMBEDDING_CONFIG")?;
        let b = std::fs::read(p).map_err(|e| format!("semantic_unavailable: {e}"))?;
        let c: Self = serde_json::from_slice(&b).map_err(|e| format!("semantic_config: {e}"))?;
        c.validate()?;
        Ok(c)
    }
    fn validate(&self) -> Result<(), String> {
        if !(1..=16).contains(&self.threads)
            || !(1..=32).contains(&self.batch_size)
            || !(64..=1500).contains(&self.chunk_chars)
            || !(100..=300_000).contains(&self.timeout_ms)
        {
            return Err("semantic_config: invalid resource bounds".into());
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String, String> {
        self.validate()?;
        let mut h = Sha256::new();
        h.update(b"m5-e5-onnx-mean-l2-v1:384:512:query:passage:chunk-v1");
        h.update(self.chunk_chars.to_le_bytes());
        for p in [
            self.model_dir.join("model.onnx"),
            self.model_dir.join("tokenizer.json"),
            self.runtime_library.clone(),
        ] {
            h.update(file_hash(&p)?);
        }
        Ok(format!("{:x}", h.finalize()))
    }
}
fn file_hash(p: &Path) -> Result<String, String> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, (u64, std::time::SystemTime, String)>>> =
        OnceLock::new();
    let m =
        std::fs::metadata(p).map_err(|e| format!("semantic_unavailable: {}: {e}", p.display()))?;
    let modified = m.modified().map_err(|e| e.to_string())?;
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .map_err(|e| e.to_string())?;
    if let Some((len, time, h)) = cache.get(p) {
        if *len == m.len() && *time == modified {
            return Ok(h.clone());
        }
    }
    let mut f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    loop {
        let n = f.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    let h = format!("{:x}", h.finalize());
    cache.insert(p.into(), (m.len(), modified, h.clone()));
    Ok(h)
}
#[derive(Debug)]
pub struct SemanticHit {
    pub path: String,
    pub score: f32,
    pub snippet: String,
    pub byte_start: usize,
    pub byte_end: usize,
}
#[derive(Debug)]
pub struct SemanticSearch {
    pub hits: Vec<SemanticHit>,
    pub indexed_revision: String,
    pub update: String,
    pub model_fingerprint: String,
    pub dimensions: usize,
}
#[derive(Serialize, Deserialize)]
struct WorkerInput {
    config: SemanticConfig,
    texts: Vec<String>,
    query: bool,
}
/// Interchangeable local adapter contract. Implementations must never transmit text.
pub trait LocalEmbeddingProvider {
    fn dimensions(&self) -> usize;
    fn fingerprint(&self) -> Result<String, String>;
    fn embed(&mut self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>, String>;
}
pub struct OnnxWorkerProvider {
    pub config: SemanticConfig,
}
impl LocalEmbeddingProvider for OnnxWorkerProvider {
    fn dimensions(&self) -> usize {
        384
    }
    fn fingerprint(&self) -> Result<String, String> {
        self.config.fingerprint()
    }
    fn embed(&mut self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>, String> {
        let exe = self
            .config
            .worker
            .clone()
            .or_else(|| std::env::var_os("MNEMOSYNE_EMBEDDING_WORKER").map(PathBuf::from))
            .or_else(|| std::env::current_exe().ok())
            .ok_or("semantic_worker_unavailable")?;
        // Files avoid pipe deadlocks for bounded but potentially large batches. Unique
        // directories have private permissions and are removed on every ordinary exit.
        let dir = std::env::temp_dir().join(format!(
            "mnemosyne-embed-{}-{}",
            std::process::id(),
            hash(format!("{:?}", Instant::now()).as_bytes())
        ));
        std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        let result = (|| {
            let input = dir.join("input");
            let output = dir.join("output");
            std::fs::write(
                &input,
                serde_json::to_vec(&WorkerInput {
                    config: self.config.clone(),
                    texts: texts.to_vec(),
                    query,
                })
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            let mut child = Command::new(exe)
                .env("TOKENIZERS_PARALLELISM", "false")
                .arg("--internal-embed-worker")
                .arg(&input)
                .arg(&output)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("semantic_worker_unavailable: {e}"))?;
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(s)) => {
                        if !s.success() {
                            return Err("semantic_worker_failed".into());
                        }
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(e.to_string());
                    }
                }
                if start.elapsed() > Duration::from_millis(self.config.timeout_ms) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("semantic_worker_timeout".into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if std::fs::metadata(&output).map_err(|e| e.to_string())?.len() > 2 * 1024 * 1024 {
                return Err("semantic_worker_output_limit".into());
            }
            let vectors: Vec<Vec<f32>> =
                serde_json::from_slice(&std::fs::read(output).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if vectors.len() != texts.len() || vectors.iter().any(|v| !valid(v, 384)) {
                return Err("semantic_invalid_vector".into());
            }
            Ok(vectors)
        })();
        let _ = std::fs::remove_dir_all(dir);
        result
    }
}
fn validate_vectors(vectors: &[Vec<f32>], count: usize, dimensions: usize) -> Result<(), String> {
    if vectors.len() != count || vectors.iter().any(|v| !valid(v, dimensions)) {
        Err("semantic_invalid_vector".into())
    } else {
        Ok(())
    }
}
fn valid(v: &[f32], dimensions: usize) -> bool {
    v.len() == dimensions
        && v.iter().all(|v| v.is_finite())
        && (v.iter().map(|x| x * x).sum::<f32>() - 1.).abs() < 0.01
}
fn chunks(text: &str, max: usize) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut start = 0;
    while start < text.len() {
        let mut end = text[start..]
            .char_indices()
            .nth(max)
            .map(|(i, _)| start + i)
            .unwrap_or(text.len());
        if end < text.len() {
            if let Some(i) = text[start..end].rfind('\n') {
                if i > (end - start) / 2 {
                    end = start + i + 1
                }
            }
        }
        if text[start..end].chars().any(char::is_alphanumeric) {
            out.push((start, end));
        }
        start = end;
    }
    out
}
fn token_chunks(
    text: &str,
    max: usize,
    tok: &tokenizers::Tokenizer,
) -> Result<Vec<(usize, usize)>, String> {
    let mut out = vec![];
    for (start, end) in chunks(text, max) {
        let mut begin = start;
        while begin < end {
            let prefixed = format!("passage: {}", &text[begin..end]);
            let encoded = tok.encode(prefixed, true).map_err(|e| e.to_string())?;
            let stop = if encoded.len() <= 512 {
                end
            } else {
                let offset = encoded.get_offsets()[510].0.saturating_sub(9);
                let mut stop = begin + offset;
                while stop > begin && !text.is_char_boundary(stop) {
                    stop -= 1;
                }
                if stop == begin {
                    return Err("semantic_chunk_token_limit".into());
                }
                stop
            };
            out.push((begin, stop));
            begin = stop;
        }
    }
    Ok(out)
}
/// Caller holds authority lock. Only current authorized paths are loaded or scored.
pub fn sync_semantic(
    root: &Path,
    tenant: &str,
    revision: &str,
    documents: &[ManagedDocument],
    allowed: &BTreeSet<String>,
    query: &str,
    config: &SemanticConfig,
    load: impl FnMut(&str) -> Result<String, String>,
) -> Result<SemanticSearch, String> {
    let mut provider = OnnxWorkerProvider {
        config: config.clone(),
    };
    sync_semantic_with_provider(
        root,
        tenant,
        revision,
        documents,
        allowed,
        query,
        config,
        load,
        &mut provider,
    )
}
/// Adapter identity must cover weights, tokenizer, prefixes, dimensions and pooling.
/// The cache namespace uses that identity so incompatible vectors cannot mix.
pub fn sync_semantic_with_provider(
    root: &Path,
    tenant: &str,
    revision: &str,
    documents: &[ManagedDocument],
    allowed: &BTreeSet<String>,
    query: &str,
    config: &SemanticConfig,
    mut load: impl FnMut(&str) -> Result<String, String>,
    provider: &mut dyn LocalEmbeddingProvider,
) -> Result<SemanticSearch, String> {
    let result = run_semantic(
        root, tenant, revision, documents, allowed, query, config, &mut load, provider,
    );
    if result
        .as_ref()
        .is_err_and(|e| e.starts_with("semantic_cache:"))
    {
        let path = root
            .join("semantic-v1")
            .join(hash(tenant.as_bytes()))
            .join("vectors.sqlite3");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-journal"));
        let mut repaired = run_semantic(
            root, tenant, revision, documents, allowed, query, config, &mut load, provider,
        )?;
        repaired.update = "rebuilt".into();
        Ok(repaired)
    } else {
        result
    }
}
fn cache_error(e: rusqlite::Error) -> String {
    format!("semantic_cache: {e}")
}
fn run_semantic(
    root: &Path,
    tenant: &str,
    revision: &str,
    documents: &[ManagedDocument],
    allowed: &BTreeSet<String>,
    query: &str,
    config: &SemanticConfig,
    mut load: impl FnMut(&str) -> Result<String, String>,
    provider: &mut dyn LocalEmbeddingProvider,
) -> Result<SemanticSearch, String> {
    config.validate()?;
    let dimensions = provider.dimensions();
    if !(1..=65536).contains(&dimensions) {
        return Err("semantic_invalid_dimensions".into());
    }
    let fingerprint = provider.fingerprint()?;
    let cache_model = format!("{fingerprint}:{dimensions}");
    let dir = root.join("semantic-v1").join(hash(tenant.as_bytes()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("vectors.sqlite3");
    let open = || -> Result<Connection, String> {
        let c = Connection::open(&path).map_err(|e| e.to_string())?;
        c.execute_batch("PRAGMA synchronous=FULL;CREATE TABLE IF NOT EXISTS vectors(model TEXT,key TEXT,data TEXT,checksum TEXT,PRIMARY KEY(model,key));CREATE TABLE IF NOT EXISTS state(model TEXT PRIMARY KEY,revision TEXT);").map_err(|e|e.to_string())?;
        c.prepare("SELECT model,key,data,checksum FROM vectors LIMIT 0")
            .map_err(|e| e.to_string())?;
        c.prepare("SELECT model,revision FROM state LIMIT 0")
            .map_err(|e| e.to_string())?;
        Ok(c)
    };
    let mut rebuilt = false;
    let c = match open() {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("sqlite3-journal"));
            rebuilt = true;
            open()?
        }
    };
    let tokenizer = tokenizers::Tokenizer::from_file(config.model_dir.join("tokenizer.json"))
        .map_err(|e| e.to_string())?;
    if tokenizer
        .encode(format!("query: {query}"), true)
        .map_err(|e| e.to_string())?
        .len()
        > 512
    {
        return Err("semantic_query_token_limit".into());
    }
    let mut work = vec![];
    let mut vectors = BTreeMap::new();
    let mut pending: BTreeMap<String, String> = BTreeMap::new();
    for doc in documents.iter().filter(|d| allowed.contains(&d.path)) {
        let text = load(&doc.blob)?;
        for (start, end) in token_chunks(&text, config.chunk_chars, &tokenizer)? {
            let body = text[start..end].to_string();
            let key = hash(
                format!("{}:{}:{}:{}", doc.blob, start, end, hash(body.as_bytes())).as_bytes(),
            );
            let stored: Option<(String, String)> = c
                .query_row(
                    "SELECT data,checksum FROM vectors WHERE model=?1 AND key=?2",
                    params![cache_model, key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(cache_error)?;
            let vector = stored.and_then(|(s, checksum)| {
                if hash(s.as_bytes()) == checksum {
                    serde_json::from_str::<Vec<f32>>(&s)
                        .ok()
                        .filter(|v| valid(v, dimensions))
                } else {
                    None
                }
            });
            if let Some(v) = vector {
                vectors.insert(key.clone(), v);
            } else {
                pending.insert(key.clone(), body.clone());
            }
            work.push((doc.path.clone(), start, end, body, key));
        }
    }
    let changed = pending.len();
    let pending: Vec<_> = pending.into_iter().collect();
    for group in pending.chunks(config.batch_size) {
        let texts: Vec<_> = group.iter().map(|(_, s)| s.clone()).collect();
        let inferred = provider.embed(&texts, false)?;
        validate_vectors(&inferred, texts.len(), dimensions)?;
        for ((key, _), v) in group.iter().zip(inferred) {
            let data = serde_json::to_string(&v).map_err(|e| e.to_string())?;
            c.execute(
                "INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3,?4)",
                params![cache_model, key, data, hash(data.as_bytes())],
            )
            .map_err(cache_error)?;
            vectors.insert(key.clone(), v);
        }
    }
    let mut query_vectors = provider.embed(&[query.to_string()], true)?;
    validate_vectors(&query_vectors, 1, dimensions)?;
    let q = query_vectors.pop().ok_or("semantic_invalid_vector")?;
    let mut best: BTreeMap<String, SemanticHit> = BTreeMap::new();
    for (path, start, end, body, key) in work {
        let v = vectors.get(&key).ok_or("semantic_missing_vector")?;
        let score = q
            .iter()
            .zip(v)
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .clamp(-1., 1.);
        if best.get(&path).is_none_or(|old| score > old.score) {
            best.insert(
                path.clone(),
                SemanticHit {
                    path,
                    score,
                    snippet: body.chars().take(360).collect(),
                    byte_start: start,
                    byte_end: end,
                },
            );
        }
    }
    c.execute("INSERT INTO state VALUES(?1,?2) ON CONFLICT(model) DO UPDATE SET revision=excluded.revision WHERE revision<>excluded.revision",params![cache_model,revision]).map_err(cache_error)?;
    let mut hits: Vec<_> = best.into_values().collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.path.cmp(&b.path)));
    Ok(SemanticSearch {
        hits,
        indexed_revision: revision.into(),
        update: if rebuilt {
            "rebuilt"
        } else if changed > 0 {
            "updated"
        } else {
            "unchanged"
        }
        .into(),
        model_fingerprint: fingerprint,
        dimensions,
    })
}
/// Invoke before normal CLI dispatch. This protocol accepts local file paths only.
pub fn worker_main(args: &[String]) -> bool {
    if args.get(1).map(String::as_str) != Some("--internal-embed-worker") {
        return false;
    }
    let result = (|| -> Result<(), String> {
        if args.len() != 4 {
            return Err("invalid_worker_args".into());
        }
        let bytes = std::fs::read(&args[2]).map_err(|e| e.to_string())?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err("worker_input_limit".into());
        }
        let input: WorkerInput = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        input.config.validate()?;
        if input.texts.len() > input.config.batch_size.max(1)
            || input.texts.iter().any(|s| s.len() > 8192)
        {
            return Err("worker_batch_limit".into());
        }
        let output = native_embed(&input)?;
        std::fs::write(
            &args[3],
            serde_json::to_vec(&output).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(70)
    }
    // Avoid process-global native ORT teardown callbacks after runtime state drops.
    #[cfg(unix)]
    unsafe {
        unsafe extern "C" {
            fn _exit(status: i32) -> !;
        }
        _exit(0)
    }
    #[cfg(not(unix))]
    std::process::exit(0)
}
fn native_embed(input: &WorkerInput) -> Result<Vec<Vec<f32>>, String> {
    use ort::{session::Session, value::Tensor};
    ort::init_from(input.config.runtime_library.display().to_string())
        .with_telemetry(false)
        .commit()
        .map_err(|e| e.to_string())?;
    let mut session = Session::builder()
        .map_err(|e| e.to_string())?
        .with_intra_threads(input.config.threads)
        .map_err(|e| e.to_string())?
        .commit_from_file(input.config.model_dir.join("model.onnx"))
        .map_err(|e| e.to_string())?;
    let mut tok = tokenizers::Tokenizer::from_file(input.config.model_dir.join("tokenizer.json"))
        .map_err(|e| e.to_string())?;
    tok.with_truncation(Some(tokenizers::TruncationParams {
        max_length: 512,
        ..Default::default()
    }))
    .map_err(|e| e.to_string())?;
    tok.with_padding(Some(tokenizers::PaddingParams::default()));
    let texts: Vec<_> = input
        .texts
        .iter()
        .map(|s| format!("{}: {s}", if input.query { "query" } else { "passage" }))
        .collect();
    let encoded = tok.encode_batch(texts, true).map_err(|e| e.to_string())?;
    let n = encoded.len();
    let len = encoded.first().ok_or("empty_batch")?.len();
    let ids: Vec<i64> = encoded
        .iter()
        .flat_map(|e| e.get_ids().iter().map(|&x| x as i64))
        .collect();
    let mask: Vec<i64> = encoded
        .iter()
        .flat_map(|e| e.get_attention_mask().iter().map(|&x| x as i64))
        .collect();
    let types: Vec<i64> = encoded
        .iter()
        .flat_map(|e| e.get_type_ids().iter().map(|&x| x as i64))
        .collect();
    let tensor = |v: Vec<i64>| Tensor::from_array(([n, len], v)).map_err(|e| e.to_string());
    let outputs=session.run(ort::inputs!["input_ids"=>tensor(ids)?,"attention_mask"=>tensor(mask.clone())?,"token_type_ids"=>tensor(types)?]).map_err(|e|e.to_string())?;
    let (shape, data) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;
    if shape.as_ref() != [n as i64, len as i64, 384] {
        return Err("semantic_model_shape".into());
    }
    let mut result = vec![];
    for b in 0..n {
        let mut v = vec![0f32; 384];
        for t in 0..len {
            if mask[b * len + t] != 0 {
                for d in 0..384 {
                    v[d] += data[(b * len + t) * 384 + d];
                }
            }
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in &mut v {
            *x /= norm;
        }
        if !valid(&v, 384) {
            return Err("semantic_invalid_vector".into());
        }
        result.push(v);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ContractProvider {
        dimensions: usize,
        malformed: u8,
        query_only: bool,
    }
    impl LocalEmbeddingProvider for ContractProvider {
        fn dimensions(&self) -> usize {
            self.dimensions
        }
        fn fingerprint(&self) -> Result<String, String> {
            Ok("adapter-contract-only".into())
        }
        fn embed(&mut self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>, String> {
            let mut v = vec![vec![0.; self.dimensions]; texts.len()];
            for row in &mut v {
                row[0] = 1.;
            }
            if self.query_only && !query {
                return Ok(v);
            }
            match self.malformed {
                1 => {
                    v.pop();
                }
                2 => v.push(vec![1.; self.dimensions]),
                3 => v[0].push(0.),
                4 => v[0][0] = f32::NAN,
                5 => v[0][0] = 2.,
                _ => {}
            }
            Ok(v)
        }
    }
    #[test]
    fn adapter_dimensions_and_every_batch_are_checked_before_cache_or_scoring() {
        // Synthetic vectors verify the adapter protocol only; actual model
        // relevance needs the separately installed E5 artifacts.
        let root =
            std::env::temp_dir().join(format!("mnemosyne-adapter-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        tokenizers::Tokenizer::new(tokenizers::models::bpe::BPE::default())
            .save(root.join("tokenizer.json"), false)
            .unwrap();
        let config = SemanticConfig {
            model_dir: root.clone(),
            runtime_library: root.join("unused"),
            worker: None,
            timeout_ms: 1000,
            threads: 1,
            batch_size: 8,
            chunk_chars: 100,
        };
        let docs = vec![ManagedDocument {
            path: "visible.md".into(),
            file_id: "f1".into(),
            blob: "b1".into(),
        }];
        let allowed = BTreeSet::from(["visible.md".into()]);
        for dimensions in [2, 3] {
            let mut provider = ContractProvider {
                dimensions,
                malformed: 0,
                query_only: false,
            };
            let r = sync_semantic_with_provider(
                &root,
                "tenant",
                "r1",
                &docs,
                &allowed,
                "question",
                &config,
                |_| Ok("source text".into()),
                &mut provider,
            )
            .unwrap();
            assert_eq!(r.dimensions, dimensions);
            assert_eq!(r.hits[0].score, 1.);
        }
        let cache = Connection::open(
            root.join("semantic-v1")
                .join(hash(b"tenant"))
                .join("vectors.sqlite3"),
        )
        .unwrap();
        let spaces: usize = cache
            .query_row("SELECT COUNT(DISTINCT model) FROM vectors", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(spaces, 2);
        drop(cache);
        for query_only in [false, true] {
            for malformed in 1..=5 {
                let mut provider = ContractProvider {
                    dimensions: 4,
                    malformed,
                    query_only,
                };
                let r = sync_semantic_with_provider(
                    &root,
                    &format!("bad-{query_only}-{malformed}"),
                    "r1",
                    &docs,
                    &allowed,
                    "question",
                    &config,
                    |_| Ok("source text".into()),
                    &mut provider,
                );
                assert!(r.unwrap_err().contains("semantic_invalid_vector"));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
