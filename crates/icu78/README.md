This crate bundles the unmodified official ICU4C 78.2 release source archive,
and upstream licensing notices. Its standard CLDR48.1 data is not linked. Instead Rust embeds the exact Chromium/Electron43.7.7 ICU data asset (CLDR48.0), also
bundled and digest-pinned. These target data weights differ for supplementary Han
characters despite both libraries reporting ICU78.2.
`PROVENANCE.json` records both official release archives, the extracted data path
and SHA-256 digests. Both compressed and uncompressed data and source digests
are verified before any build. The selected version matches the reference runtime;
78.3 is intentionally not substituted.

Cargo builds static common and internationalization libraries plus the
upstream empty data stub. The exact target data bytes are embedded as an aligned
Rust static and registered before any ICU data operation; Chromium trimmed data
is intentionally not repackaged by ICU tools. Native builds require a C11/C++17 compiler, POSIX shell, make, tar and gzip.
No Python, network download, installed desktop app or system ICU is used. The
libraries/data are linked into the final binary. ICU entry points have a custom
`_78_mnemosyne` suffix to isolate this dependency from other ICU consumers.
Runtime initialization disables filesystem data lookup and registers the
embedded data. It verifies the linked version is exactly 78.2.0.0.

Native macOS and Linux build paths use the upstream configure/make build. macOS
arm64 has been exercised against the pinned reference corpus; Linux builds and
other macOS architectures require independent build/packaging checks. Windows
MSVC and cross compilation currently fail explicitly, rather than silently
linking host/system ICU. Those pipelines remain required work: build the same
source/data with a target compiler, retain versioned/suffixed symbols and data,
include LICENSE-ICU, LICENSE-Electron and the complete Chromium license notices
(decompress LICENSES.chromium.html.gz) in distributions, and run the same conformance corpus.
Release bundles must distribute all upstream license notices; all ICU symbols/data
stay inside the executable or Rust library, so no runtime ICU DLL search is
part of the present static packaging.
