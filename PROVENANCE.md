# Extraction provenance

Seatline was extracted from `davletovb/TabBeam` at source commit
`b4bfd5bd0f3ca9db461963b971b8af0177754e5d`.

The 73 runtime, test, and fuzz files transferred from TabBeam were created in
this repository from the exact source contents. Because Git blobs are
content-addressed, their blob SHAs are identical in both repositories.

The GitHub connector used for the extraction exposes Git object creation and
ref updates, but not an authenticated Git transport or history-filter operation
equivalent to `git filter-repo`. The original pre-extraction commit history is
therefore retained in TabBeam and linked by the source commit above rather than
being reconstructed with invented author/date metadata.

Repository-level workspace, CI, provenance, and licensing files were added for
the standalone library.
