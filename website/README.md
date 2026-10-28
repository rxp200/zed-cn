# Zed CN website

Static product website built with Vite and locally bundled Three.js. No backend, analytics, CDN runtime or remote fonts. The editor illustration is explicitly labeled as an illustration.

## Local development

```sh
npm ci
npm run dev
npm test
npm run build
```

The production build is `dist/`. Relative asset URLs support GitHub Pages' `/zed-cn/` project path. Serve it over HTTP rather than opening `index.html` through `file://`.

## Production integration

The existing `Publish update manifest` workflow generates completed Stable and optional Dev release feeds, then builds this website and combines both in one Pages artifact using `script/assemble-pages.py`. Do not add a second independent Pages deploy workflow: Pages deployment replaces the complete site and could remove the client's update feeds.

Downloads use same-origin `updates.json` and `updates-dev.json`. Assets must have exact project download URLs, positive sizes, uploaded state and SHA-256 digests. Selection sorts versions numerically and retains honest per-platform historical fallbacks. Missing feeds show a GitHub Releases fallback. No version is hardcoded in production content.

## Browser regression

Serve the assembled artifact with real feeds at a project subpath, then:

```sh
SITE_URL=http://127.0.0.1:4173/zed-cn/ CHROMIUM_PATH=/path/to/chromium npm run test:browser
```

Alternatively set `CDP_URL` to an existing isolated browser debugging endpoint. The test verifies desktop/mobile overflow, navigation, keyboard tabs, live channel selection, no-JavaScript, reduced-motion and feed failure fallback. It expects the checked deployment to contain five desktop assets in Stable and a completed Dev feed; those assertions describe this project's verified current release fixtures, not every future partial release.

## Motion and privacy

Three.js is lazy-loaded, omitted for reduced-motion/data-saving visitors, pauses offscreen/background and provides a pause button. Geometry is bounded and renderer pixel ratio capped at 1.5. WebGL failure leaves a CSS illustration. No desktop or Remote Server code is modified. Third-party license text is in `public/third-party-notices.txt`.

Product copy reflects current source, not an assertion every older Stable build includes every capability. Zed CN is an unofficial community fork, not endorsed by Zed Industries. GPL/Apache license and Release links remain available.
