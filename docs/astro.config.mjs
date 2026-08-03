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
      description: 'The self-hosted power frontend for Codex, with remote control, voice, subagents, worktrees, and adversarial review.',
      head: [
        { tag: 'link', attrs: { rel: 'preconnect', href: 'https://fonts.googleapis.com' } },
        { tag: 'link', attrs: { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: true } },
        {
          tag: 'link',
          attrs: {
            rel: 'stylesheet',
            href: 'https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500;600;700;800&family=Rajdhani:wght@400;500;600;700&family=Staatliches&display=swap',
          },
        },
        { tag: 'meta', attrs: { property: 'og:type', content: 'website' } },
        { tag: 'meta', attrs: { property: 'og:image', content: socialCardUrl } },
        { tag: 'meta', attrs: { property: 'og:image:type', content: 'image/png' } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        {
          tag: 'meta',
          attrs: {
            property: 'og:image:alt',
            content: 'Mjolnir: the self-hosted power frontend for Codex, with an ASCII-art hammer.',
          },
        },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: socialCardUrl } },
        {
          tag: 'meta',
          attrs: {
            name: 'twitter:image:alt',
            content: 'Mjolnir: the self-hosted power frontend for Codex, with an ASCII-art hammer.',
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
          label: 'Start with Codex',
          items: [
            { label: 'Why Mjolnir for Codex', slug: 'codex' },
            { label: 'Install and run', slug: 'install' },
            { label: '10-minute Codex evaluation', slug: 'evaluate' },
            { label: 'Data and trust boundaries', slug: 'data-boundaries' },
          ],
        },
        {
          label: 'Codex workflows',
          items: [
            { label: 'Remote control', slug: 'remote' },
            { label: 'Voice dictation', slug: 'voice' },
            { label: 'Subagents', slug: 'subagents' },
            { label: 'Delegation and adversarial review', slug: 'delegation-review' },
            { label: 'Permissions and workspace scope', slug: 'permissions' },
            { label: 'Sessions, worktrees, and resume', slug: 'sessions-worktrees' },
            { label: 'Headless automation', slug: 'headless' },
          ],
        },
        {
          label: 'Extend Mjolnir',
          items: [
            { label: 'Other agents and models', slug: 'adapters' },
            { label: 'Configuration', slug: 'configuration' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI and keyboard', slug: 'cli-reference' },
            { label: 'Architecture and boundaries', slug: 'overview' },
            { label: 'Storage and network activity', slug: 'storage-network' },
            { label: 'License and use cases', slug: 'license-use-cases' },
            { label: 'Third-party notices', slug: 'third-party-notices' },
          ],
        },
      ],
    }),
  ],
});
