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
        baseUrl: 'https://github.com/nrynss/lambo/edit/main/site',
      },
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'Quickstart', slug: 'quickstart' },
            { label: 'Demo', slug: 'demo' },
          ],
        },
        {
          label: 'References',
          items: [
            { label: 'Installation', slug: 'installation' },
            { label: 'MCP tools', slug: 'mcp' },
            { label: 'Agent skill', slug: 'agent-skill' },
            { label: 'Command line', slug: 'cli' },
            { label: 'Configuration', slug: 'config' },
            { label: 'Library API', slug: 'api' },
            { label: 'End to end', slug: 'end-to-end' },
            { label: 'Evidence & evaluation', slug: 'evidence' },
            { label: 'Origin', slug: 'origin' },
            { label: 'Hackathon submission', slug: 'hackathon' },
          ],
        },
      ],
    }),
  ],
});
