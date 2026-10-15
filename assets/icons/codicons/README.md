# VS Code codicons

Monochrome icon artwork taken from
[microsoft/vscode-codicons](https://github.com/microsoft/vscode-codicons) through
the published `@vscode/codicons` package, version `0.0.45` (font version `1.15`).
The files are copied unmodified from the package's `src/icons` directory, or
rendered from its `dist/codicon.svg` sprite when the source directory does not
ship a standalone file for an icon.

`crates/icons/src/icons.rs` maps the workbench chrome `IconName` variants onto
these files with `IconName::codicon()`. `IconName` variants that are not mapped
(product and AI brand marks, file-type icons, window-control glyphs that have no
codicon counterpart) keep their upstream Zed artwork under `assets/icons`.

- `LICENSE-CC-BY-4.0.txt` covers the icon artwork.
- `LICENSE-MIT.txt` covers the codicons build tooling.

To add an icon, drop the codicon here and add the mapping in
`crates/icons/src/icons.rs`; the `icons` crate tests fail if a codicon file is
unused or a mapped file is missing.
