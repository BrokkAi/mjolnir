import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import rehypeBasePathLinks from './rehype-base-path-links.mjs';

const site = process.env.PUBLIC_DOCS_SITE ?? 'https://mjolnir.brokk.ai';
const productionBase = process.env.PUBLIC_DOCS_BASE ?? '/';
const isDev = process.argv.includes('dev');
const base = isDev ? '/' : productionBase;
const socialCardPath = [productionBase.replace(/^\/+|\/+$/g, ''), 'og.png']
  .filter(Boolean)
  .join('/');
const socialCardUrl = new URL(`/${socialCardPath}`, site).href;

export default defineConfig({
  site,
  base,
  markdown: {
    processor: unified({
      rehypePlugins: [[rehypeBasePathLinks, { base }]],
    }),
  },
  integrations: [
    starlight({
      title: 'Mjolnir',
      description: 'A forge-grade terminal client for a council of coding agents.',
      head: [
        { tag: 'meta', attrs: { property: 'og:type', content: 'website' } },
        { tag: 'meta', attrs: { property: 'og:image', content: socialCardUrl } },
        { tag: 'meta', attrs: { property: 'og:image:type', content: 'image/png' } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        {
          tag: 'meta',
          attrs: {
            property: 'og:image:alt',
            content: 'Mjolnir: one terminal, a council of agents, with an ASCII-art hammer.',
          },
        },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: socialCardUrl } },
        {
          tag: 'meta',
          attrs: {
            name: 'twitter:image:alt',
            content: 'Mjolnir: one terminal, a council of agents, with an ASCII-art hammer.',
          },
        },
      ],
      customCss: ['./src/styles/mjolnir.css'],
      components: {
        Header: './src/components/MjolnirHeader.astro',
        Hero: './src/components/MjolnirHero.astro',
      },
      favicon: '/favicon.svg',
      editLink: {
        baseUrl: 'https://github.com/BrokkAi/mjolnir/edit/master/docs/',
      },
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/BrokkAi/mjolnir',
        },
      ],
      sidebar: [
        {
          label: 'Start',
          items: [
            { label: 'Overview', slug: 'overview' },
            { label: 'Install and run', slug: 'install' },
            { label: '10-minute evaluation', slug: 'evaluate' },
            { label: 'License and use cases', slug: 'license-use-cases' },
            { label: 'Data and trust boundaries', slug: 'data-boundaries' },
          ],
        },
        {
          label: 'The Council',
          items: [
            { label: 'Thor, Eitri, and Loki', slug: 'council' },
            { label: 'ACP adapters and models', slug: 'adapters' },
            { label: 'Delegation and review', slug: 'delegation-review' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Configuration', slug: 'configuration' },
            { label: 'Permissions and workspace scope', slug: 'permissions' },
            { label: 'Sessions, worktrees, and resume', slug: 'sessions-worktrees' },
            { label: 'Headless automation', slug: 'headless' },
            { label: 'Remote control', slug: 'remote' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI and keyboard', slug: 'cli-reference' },
            { label: 'Storage and network activity', slug: 'storage-network' },
            { label: 'Third-party notices', slug: 'third-party-notices' },
          ],
        },
      ],
    }),
  ],
});
