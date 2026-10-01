# Zed CN modification notice

Zed CN is an unofficial modified distribution of [Zed](https://github.com/zed-industries/zed). It is not affiliated with, sponsored by, or endorsed by Zed Industries, Inc.

- Upstream project: `zed-industries/zed`
- Modified distribution: `rxp200/zed-cn`
- Public modification history began in 2026.
- The exact upstream baseline, modification dates, authorship, and changes for a release are recorded in its Git history and GitHub Release notes.
- Each Zed CN Release identifies the exact source commit used to build its binaries.

Zed and most first-party crates are licensed under GPL-3.0-or-later. Components explicitly marked Apache-2.0 remain under Apache-2.0. Vendored and other third-party components remain under their respective licenses. See `LICENSE-GPL`, `LICENSE-APACHE`, per-component license files, and the generated third-party license report distributed with desktop packages.

Zed CN preserves upstream copyright, patent, trademark, and attribution notices. Zed and the Zed logo are trademarks of their respective owner; open-source licenses do not grant permission to imply that this modified distribution is an official Zed Industries release.

## Apache-2.0 files modified by Zed CN

The following Apache-2.0 source files carry an in-file modification notice. Their detailed changes and dates are available from Git history:

- `crates/alacritty_terminal/src/event_loop.rs`
- `crates/gpui/src/app.rs`
- `crates/gpui/src/elements/div.rs`
- `crates/gpui/src/platform/threaded_dispatcher.rs`
- `crates/gpui/src/window.rs`
- `crates/gpui_linux/src/linux/dispatcher.rs`
- `crates/gpui_linux/src/linux/platform.rs`
- `crates/gpui_windows/src/platform.rs`
- `crates/gpui_windows/src/window.rs`
- `crates/util/src/path_list.rs`

The vendored `crates/alacritty_terminal` source is based on Zed Industries' Alacritty fork revision `4c129667ce56611becdc82de6e28218c80e2e88f`. Except for the file listed above, the vendored source is preserved from that revision. Its original Apache-2.0 and MIT license texts remain in the component directory.

## Corresponding source

For desktop and Remote Server binaries distributed through GitHub Releases, use the exact source commit linked in that Release. The tagged source contains the build scripts, manifests, lockfile, interface definitions, and other project-controlled material needed to build the covered binaries. Standalone compressed Remote Server assets are accompanied by this source offer and the license links in the same Release, even when a platform's single-file compression format cannot embed additional notice files. General-purpose toolchains, operating-system components, and separately licensed third-party dependencies remain subject to their own distribution terms.

No additional restriction in an installer notice or project document limits the rights granted by the applicable open-source license. Official Zed accounts and online services, if used, remain subject to Zed Industries' separate service terms.
