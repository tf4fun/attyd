# Third-party license materials

Release archives contain the project's Apache-2.0 `LICENSE` and a generated
`THIRD_PARTY_LICENSES.txt`. Generate the latter after `npm ci` with:

```bash
npm run licenses -- x86_64-unknown-linux-gnu
```

The script reads locked Cargo metadata for the chosen target and installed
production npm packages. It preserves their license, copyright, and NOTICE
files, including nested licenses for bundled native code. Rust development-only
dependencies are excluded; build dependencies and production npm packages may
include code that is not present in the final binary.

The pinned ACP SDK's license is at its workspace root. A small, version-specific
list of crates that omit license files uses the Apache-2.0 option explicitly
declared in their metadata. Missing materials for other dependencies fail the
generation step and require review; no generic NOTICE is invented.

The Apache-2.0 exceptions were checked against the published crate manifests
and their recorded upstream commits. These commits contain no NOTICE file:

| Crate version | Declared license and source |
| --- | --- |
| `agent-client-protocol-schema 1.7.0` | Apache-2.0; [source](https://github.com/agentclientprotocol/agent-client-protocol/tree/272bf799f35a258c6a4107a0410ed361e83683d3). |
| `defmt-parser 1.0.0` | MIT OR Apache-2.0; [source](https://github.com/knurling-rs/defmt/tree/4a8cdb44891ed57b8ff5a023b6bec7137c48708f). |
| `eventsource-stream 0.2.3` | MIT OR Apache-2.0; [manifest](https://github.com/jpopesculian/eventsource-stream/blob/3d46f1c758f9ee4681e9da0427556d24c53f9c01/Cargo.toml). |
| `include-flate-codegen 0.3.4`, `include-flate-compress 0.3.4` | Apache-2.0; [source](https://github.com/SOF3/include-flate/tree/4118d8e5d30b81c678f889b492945c39c019f846). |

`alloc-stdlib-0.2.4.txt` supplies the BSD-3-Clause text omitted from that crate's
archive. It is copied unchanged from its published source commit:
[Dropbox license](https://github.com/dropbox/rust-alloc-no-stdlib/blob/ae42d22078b98549e987d2f03d12df7b984fde47/LICENSE).

Review these exceptions and the generated materials when updating dependencies.
Agents and externally configured MCP servers are separately installed software
and are not part of attyd's release archives.
