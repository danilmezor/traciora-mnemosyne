# Synthetic extraction fixtures

The documents were generated deterministically with the Python 3 standard library
(`zipfile`, `io`, `hashlib`, `json`); `manifest.json` records size and SHA-256 per document.
No customer or personal content is used.

`mixed.pdf` has two pages of English and Russian text using a synthetic Type0
font and a ToUnicode character map containing only used characters. It is a parser fixture, not a typography
reference. `structured.docx` has a heading, ordinary paragraphs, a table paragraph
and a section boundary. `replacement.pdf` provides visibly different source text.
`partial.pdf` has one text page and one graphics-only page. `image-only.pdf`
contains a filled rectangle and no text; it represents the no-text/OCR-deferred
class without requiring raster image libraries. `encrypted.pdf` has a Standard
security dictionary and is used only to assert encrypted-source rejection.
Malformed and empty containers exercise explicit parser outcomes.
`bounded-expansion.docx` compresses a finite 9 MiB repetitive XML member to a
small ZIP, exceeding the 8 MiB document XML cap. It is a bounded expansion test,
not an unbounded or recursively nested archive. `doctype.docx` deliberately includes a DOCTYPE/entity declaration pointing at
a nonexistent local synthetic fixture path to check declaration rejection without
resolving it. `malformed-xml.docx` has unclosed main-document XML. No fixture
includes macros, embedded executables, customer content or network calls.

`alternate-prefix.docx` binds the standard WordprocessingML namespace to `alt`
and uses that valid alternate prefix throughout. `cdata.docx` puts RU/EN text and
a literal ampersand inside an XML CDATA section. Both must produce searchable
paragraph text, preserving XML namespace equivalence and literal CDATA content.
