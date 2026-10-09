# README artwork

The README illustrations are original HTML/CSS compositions using the Client UI's Google-style dark theme and fonts. They are drawn as diagrams, without application screenshots. `index.html`, `styles.css`, `artwork.js`, and `connections.js` contain the editable source. They use four views in English and Chinese:

| View           | Subject                                   |
| -------------- | ----------------------------------------- |
| `hero`         | Agent definition, packaging, and execution |
| `build`        | Building and packaging an agent           |
| `architecture` | Core, HTTP server, client, and runtimes    |
| `swe`          | Software engineering task walkthrough     |

Each page renders one 1,200 CSS-pixel-wide `[data-artboard]`. The artwork script sets `document.documentElement.dataset.ready = 'true'` after inserting content, loading fonts, and drawing connectors. The illustrations explain the product; task text and results are examples, not records of a model run.

GitHub does not execute repository HTML or CSS inside a Markdown README. The READMEs therefore embed PNGs generated from these sources, at `docs/images/readme/{view}-{lang}.png`. The PNGs are rendered at 2× resolution; editing the sources and regenerating them keeps both languages consistent.

## Generate the images

From the repository root, install the existing web tooling and Chromium:

```bash
npm --prefix apps/web ci
cd apps/web
npx playwright install chromium
cd ../..
```

Render all eight images:

```bash
npm --prefix apps/web run docs:images
```

Render only one view and language, or put a draft outside the repository:

```bash
npm --prefix apps/web run docs:images -- --view hero --lang en
npm --prefix apps/web run docs:images -- --view swe --lang zh --out-dir /tmp/aporto-readme
```

`--out-dir` accepts an absolute path or a path relative to the command's working directory. The default output directory always resolves from the repository root. Running through `npm --prefix apps/web` makes the working directory `apps/web`, so an absolute draft path is the clearest option.

The renderer starts an isolated server on a random `127.0.0.1` port. It uses Playwright from `apps/web/node_modules`, waits for the ready flag, fonts, images, and network idle, and checks resource failures, browser errors, artboard width, and overflow before writing each PNG. External resource requests fail the render. The browser and temporary server close when rendering finishes or is interrupted. No application server, runtime, model, or credential is needed.

## Preview and edit

```bash
npm --prefix apps/web run docs:images -- --serve --view architecture --lang en
```

Open the printed localhost URL. Refresh it after editing the source; responses are not cached. Stop the preview with `Ctrl+C`. Add `--port 4328` if you need a fixed local port. Omitting `--view` or `--lang` prints links for all selected combinations.

The HTML entry accepts `?view=hero|build|architecture|swe&lang=en|zh`. Keep visible text inside the artboard. Clip intentional decorative overflow inside its own container rather than extending the artboard's scrollable area. Inspect the generated images before committing changes, especially text wrapping in both languages.

The cover uses a CSS grid for its resource lifecycle. Build and architecture connectors are defined by source/target card selectors in `artwork.js`; `connections.js` derives their endpoints from the rendered card bounds. Adjust layout in CSS instead of hardcoding arrow coordinates. Keep resource lifecycle arrows separate from execution calls: Client UI calls the Server HTTP API, while CLI invokes the agent engine directly.

Add `data-layout-bounds` to cards or other containers whose content must fit inside them. The renderer checks each marked container's scrollable dimensions and requires visible text to stay inside its nearest marked ancestor, with a 2 CSS-pixel tolerance. The optional attribute value names the container in error messages, for example `data-layout-bounds="build-step"`. This catches text that stays inside the overall artboard but spills out of its card; it does not depend on a particular CSS class.

## Shared theme, fonts, and local assets

The artwork imports the Client UI's [theme](../../apps/web/src/theme.css) and [font declarations](../../apps/web/src/fonts.css) directly. The shared theme defines the Google Sans Flex / Noto Sans SC font stack, dark surfaces, text and accent colors, and Aporto's brand gradient. Use these variables for artwork styling so changes stay aligned with the Client UI; keep diagram layout rules in `styles.css`.

The temporary server exposes exactly the two shared CSS files, the artwork directory, and the installed font packages:

| URL                                              | Local source                                                   |
| ------------------------------------------------ | -------------------------------------------------------------- |
| `/client/theme.css` (exact file)                 | `apps/web/src/theme.css`                                       |
| `/client/fonts.css` (exact file)                 | `apps/web/src/fonts.css`                                       |
| `/docs/visuals/`                                 | `docs/visuals/`                                                |
| `/client/@fontsource-variable/google-sans-flex/` | `apps/web/node_modules/@fontsource-variable/google-sans-flex/` |
| `/client/@fontsource-variable/noto-sans-sc/`     | `apps/web/node_modules/@fontsource-variable/noto-sans-sc/`     |

The font CSS is served unchanged: its relative package URLs resolve through these mappings. Other Client UI source files are not exposed. Directory traversal and symlink escapes are rejected. The renderer uses the browser's dark color scheme and waits for all required fonts before export.

Font assets are self-hosted; rendering does not contact Google Fonts or a CDN. Install dependencies before an offline render. The pinned packages and the full redistribution licenses are documented in [Web fonts](../fonts.md), with the original [Google Sans Flex](../../apps/web/public/licenses/google-sans-flex.txt) and [Noto Sans SC](../../apps/web/public/licenses/noto-sans-sc.txt) notices retained in the repository.
