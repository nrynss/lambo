import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// The site deploys to GitHub Pages at https://nrynss.github.io/lambo.
// `base` matches the repository name so assets resolve under that path.
export default defineConfig({
  site: 'https://nrynss.github.io',
  base: '/lambo',
  integrations: [
    starlight({
      title: 'Lambo',
      description: 'Agentic graph memory for multi-agent coding.',
      favicon: '/favicon.svg',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/nrynss/lambo' },
      ],
      editLink: {
        baseUrl: 'https://github.com/nrynss/lambo/edit/phase/p8-surface/site',
      },
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'Quickstart', link: '/quickstart' },
            { label: 'Demo', link: '/demo' },
          ],
        },
        {
          label: 'References',
          items: [
            { label: 'Installation', link: '/installation' },
            { label: 'MCP tools', link: '/mcp' },
            { label: 'Command line', link: '/cli' },
            { label: 'Configuration', link: '/config' },
            { label: 'Library API', link: '/api' },
            { label: 'End to end', link: '/end-to-end' },
          ],
        },
      ],
    }),
  ],
});
