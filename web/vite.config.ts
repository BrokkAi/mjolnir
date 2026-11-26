import { fileURLToPath } from 'node:url';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// The compiled app is checked into mj-server/src/web/dist and embedded by the
// Rust crate, so `cargo build` never needs Node. The server's CSP is
// `script-src 'self'; style-src 'self'; font-src 'self'`, so nothing may be
// inlined: every asset is a hashed file under /assets/.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  build: {
    outDir: '../mj-server/src/web/dist',
    emptyOutDir: true,
    assetsDir: 'assets',
    // data: URIs are not allowed for fonts, and inlining would defeat caching.
    assetsInlineLimit: 0,
    cssCodeSplit: true,
    sourcemap: false,
    target: 'es2022',
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes('node_modules')) return undefined;
          if (/node_modules\/(react|react-dom|scheduler)\//.test(id)) return 'react';
          if (/node_modules\/(react-markdown|remark-[^/]+|rehype-[^/]+|unified|mdast-[^/]+|hast-[^/]+|micromark[^/]*|unist-[^/]+|vfile[^/]*|property-information|space-separated-tokens|comma-separated-tokens|decode-named-character-reference|character-entities[^/]*|trim-lines|trough|bail|is-plain-obj|devlop|html-url-attributes|estree-util-[^/]+|ccount|markdown-table|zwitch|longest-streak|escape-string-regexp|parse-entities|stringify-entities)\//.test(id)) return 'markdown';
          return undefined;
        },
      },
    },
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8787',
      '/auth': 'http://127.0.0.1:8787',
    },
  },
});
