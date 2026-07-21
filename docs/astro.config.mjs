import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import rehypeBasePathLinks from './rehype-base-path-links.mjs';

const site = process.env.PUBLIC_DOCS_SITE ?? 'https://brokkai.github.io';
const productionBase = process.env.PUBLIC_DOCS_BASE ?? '/mjolnir';
const isDev = process.argv.includes('dev');
const base = isDev ? '/' : productionBase;

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
          ],
        },
        {
          label: 'The Council',
          items: [
            { label: 'Thor, Eitri, and Loki', slug: 'council' },
            { label: 'Configuration', slug: 'configuration' },
          ],
        },
        {
          label: 'Workflows',
          items: [
            { label: 'Remote and headless', slug: 'remote' },
          ],
        },
      ],
    }),
  ],
});
