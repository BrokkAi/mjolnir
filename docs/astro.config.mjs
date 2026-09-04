import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import rehypeBasePathLinks from './rehype-base-path-links.mjs';

const site = process.env.PUBLIC_DOCS_SITE ?? 'https://mjolnir.brokk.ai/';
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
      description: 'A terminal control plane for long-running ACP coding-agent sessions, with local, container, and remote targets.',
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
            content: 'Mjolnir: a terminal control plane for Codex, Claude Code, Kimi Code, Grok Build, and DeepSeek Harness sessions.',
          },
        },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: socialCardUrl } },
        {
          tag: 'meta',
          attrs: {
            name: 'twitter:image:alt',
            content: 'Mjolnir: a terminal control plane for Codex, Claude Code, Kimi Code, Grok Build, and DeepSeek Harness sessions.',
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
          label: 'Containers',
          items: [
            { label: 'Container targets', slug: 'containers' },
            { label: 'Podman for Mjolnir', slug: 'podman' },
            { label: 'Docker for Mjolnir', slug: 'docker' },
            { label: 'Custom container images', slug: 'custom-images' },
          ],
        },
      ],
    }),
  ],
});
