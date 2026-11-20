import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import remarkMath from 'remark-math';
import rehypeKatex from 'rehype-katex';

// The site deploys to GitHub Pages at https://nrynss.github.io/lambo.
// `base` matches the repository name so assets resolve under that path.
export default defineConfig({
  site: 'https://nrynss.github.io',
  base: '/lambo',
  // KaTeX renders the canonization formulas on /canonization/. `$...$` inline
  // and `$$...$$` display, the same syntax GitHub renders in the README.
  markdown: {
    remarkPlugins: [remarkMath],
    rehypePlugins: [rehypeKatex],
  },
  integrations: [
    starlight({
      title: 'Lambo',
      description:
        'Shared, self-consolidating graph memory for coding agents and multi-agent AI operations.',
      favicon: '/favicon.svg',
      logo: {
        src: './src/assets/lambo-mark.svg',
        alt: 'Lambo',
      },
      head: [
        {
          tag: 'link',
          attrs: {
            rel: 'preconnect',
            href: 'https://fonts.googleapis.com',
          },
        },
        {
          tag: 'link',
          attrs: {
            rel: 'preconnect',
            href: 'https://fonts.gstatic.com',
            crossorigin: '',
          },
        },
        {
          tag: 'link',
          attrs: {
            rel: 'stylesheet',
            href: 'https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:ital,wght@0,400;0,500;0,600;0,700;1,400&display=swap',
          },
        },
      ],
      customCss: [
        'katex/dist/katex.min.css',
        './src/styles/custom.css',
      ],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/nrynss/lambo' },
      ],
      editLink: {
        baseUrl: 'https://github.com/nrynss/lambo/edit/main/site',
      },
      // Five small groups rather than one eleven-item 'References' bucket: a
      // reader can tell from the group label whether a page is for running
      // Lambo, calling it, or checking a claim.
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'Quickstart', slug: 'quickstart' },
            { label: 'Demo', slug: 'demo' },
            { label: 'Origin', slug: 'origin' },
          ],
        },
        {
          label: 'Install and configure',
          items: [
            { label: 'Installation', slug: 'installation' },
            { label: 'macOS multi-session setup', slug: 'macos' },
            { label: 'Configuration', slug: 'config' },
            { label: 'Migration and memory repair', slug: 'migration' },
          ],
        },
        {
          label: 'Interfaces',
          items: [
            { label: 'MCP tools', slug: 'mcp' },
            { label: 'Command line', slug: 'cli' },
            { label: 'Agent skill', slug: 'agent-skill' },
            { label: 'Library API', slug: 'api' },
          ],
        },
        {
          label: 'How it works',
          items: [
            { label: 'Canonization', slug: 'canonization' },
            { label: 'End to end', slug: 'end-to-end' },
          ],
        },
        {
          label: 'Evidence',
          items: [
            { label: 'Evidence & evaluation', slug: 'evidence' },
          ],
        },
      ],
    }),
  ],
});
