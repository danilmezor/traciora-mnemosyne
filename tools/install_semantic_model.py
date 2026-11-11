#!/usr/bin/env python3
"""Explicit opt-in installer for the pinned semantic search model and ONNX runtime."""
import argparse, hashlib, json, os
from pathlib import Path
import platform, subprocess, tarfile, tempfile

MANIFEST=Path(__file__).resolve().parent/'semantic_model.json'
def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()
def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--destination',required=True,type=Path)
    p.add_argument('--accept-download-and-licenses',action='store_true',required=True,
                   help='Explicitly download the model/runtime under their MIT licenses')
    a=p.parse_args()
    if (platform.system(),platform.machine())!=('Darwin','arm64'):
        p.error('Pinned runtime supports macOS arm64 only; supply your own compatible local ONNX Runtime elsewhere.')
    destination=a.destination.resolve(); destination.mkdir(parents=True,exist_ok=True)
    manifest=json.loads(MANIFEST.read_text())
    for artifact in manifest['artifacts']:
        target=destination/artifact['name']
        if target.is_file() and sha(target)==artifact['sha256']: continue
        with tempfile.NamedTemporaryFile(dir=destination,delete=False) as f: temporary=Path(f.name)
        try:
            subprocess.run(['curl','--fail','--location','--retry','2','--proto','=https','--output',str(temporary),artifact['url']],check=True)
            if sha(temporary)!=artifact['sha256']: raise RuntimeError('SHA-256 mismatch: '+artifact['name'])
            os.replace(temporary,target)
        finally:
            temporary.unlink(missing_ok=True)
    with tarfile.open(destination/'runtime.tgz') as archive:
        for member in archive.getmembers():
            resolved=(destination/member.name).resolve()
            if not resolved.is_relative_to(destination): raise RuntimeError('Unsafe archive path')
            if member.issym() or member.islnk():
                linked=(resolved.parent/member.linkname).resolve()
                if not linked.is_relative_to(destination): raise RuntimeError('Unsafe archive link')
        archive.extractall(destination)
    runtime=destination/manifest['runtime_library']
    if sha(runtime)!=manifest['runtime_library_sha256']: raise RuntimeError('Extracted runtime hash mismatch')
    # Exact provider fields are documented alongside the runtime implementation.
    config={'model_dir':str(destination),'runtime_library':str(runtime),'timeout_ms':30000,'threads':2,'batch_size':8,'chunk_chars':1000}
    (destination/'config.json').write_text(json.dumps(config,indent=2)+'\n')
    (destination/'installation.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print(destination/'config.json')
if __name__=='__main__': main()
