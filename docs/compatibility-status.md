# Compatibility status

This file records compatibility evidence for externally observable download
behavior. An unlisted behavior has not been audited here.

## HTTP(S) multi-mirror range resume

- **Upstream format:** aria2 documents `.aria2` in-flight piece records as an
  index, piece length, and block bitfield whose bits represent 16 KiB chunks.
  See the [official control-file format](https://aria2.github.io/manual/en/html/technical-notes.html#control-file-aria2-format).
- **Supported scope:** known-length multi-mirror HTTP(S) downloads restore
  in-flight blocks only when the sidecar layout and per-piece geometry match.
  The downloader reuses only the leading contiguous run of complete blocks;
  blocks after a gap are fetched again. The aggregate `completed_length` is
  not treated as a file prefix for a split layout.
- **Durability rule:** output writes are synchronized before in-flight records
  are saved. Completed-piece bits are published only after the corresponding
  output has crossed the same sync barrier.
- **Verification:** engine-level tests pause and resume a multi-mirror download,
  check persisted blocks against the output bytes, force mirror reassignment,
  reject a discontiguous bitmap, and compare the final output byte-for-byte.
- **Known gap:** the upstream `aria2c` executable was not available in the
  validation environment, so cross-version resume with the original binary was
  not exercised. The project parser and writer use the documented native
  in-flight record layout, but this does not establish an end-to-end
  cross-binary interoperability result.
