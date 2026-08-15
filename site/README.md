# Lambo docs site

The Lambo documentation site. It is an [Astro](https://astro.build) site built with the [Starlight](https://starlight.astro.build) theme, and it deploys to GitHub Pages at `https://nrynss.github.io/lambo`.

The site lives in `site/`. It is self-contained and does not touch the Rust build.

## Layout

- `src/content/docs/index.mdx` is the landing page. It carries the badges, the architecture diagram, and the project narrative.
- `src/content/docs/quickstart.mdx` and `src/content/docs/demo.mdx` are the getting-started pages.
- `src/content/docs/*.mdx` are the six reference pages, copied from `docs/reference/`. They are the source of truth.
- `src/components/` holds small components. `Mermaid.astro` renders the architecture diagram client-side. The `Mdx*` components map the reference pages' block styles onto Starlight.
- `src/content.config.ts` wires the Starlight content collection.
- `src/public/favicon.svg` is the site icon.

## Run locally

You need Node.js 20 or newer and npm.

```bash
cd site
npm install
npm run dev
```

Open the printed URL, which is `http://localhost:4321/lambo`. The dev server reloads on edit.

To build a static copy and serve it:

```bash
npm run build
npm run preview
```

`npm run build` writes the site to `site/dist`.

## Deploy to GitHub Pages

The workflow `site/.github/workflows/deploy.yml` builds the site and deploys it with the official Pages actions.

GitHub only auto-detects workflows from the repository root `.github/workflows/`. To make the deploy run automatically when changes land on `main`, copy the workflow to the repository root:

```bash
cp site/.github/workflows/deploy.yml .github/workflows/docs.yml
```

The copy inherits the repo root, so remove the `working-directory: site` default from the build job or point it at `site` from the root. Then enable Pages once:

1. Open the repository **Settings**.
2. Choose **Pages**.
3. Under **Build and deployment**, set **Source** to **GitHub Actions**.

The first deploy runs on the next push to `main`. You can also trigger it manually from the **Actions** tab. The live site is `https://nrynss.github.io/lambo`.

## Reference content

Reference pages are copied from `docs/reference/`. When those change, re-copy them into `src/content/docs/` so the site stays current. Do not edit the reference exports in place.
