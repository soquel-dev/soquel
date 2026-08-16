// @ts-check
import sitemap from '@astrojs/sitemap'
import tailwindcss from '@tailwindcss/vite'
import { defineConfig } from 'astro/config'

export default defineConfig({
  site: 'https://soquel.dev',
  // Canonical URLs carry no trailing slash; keeps sitemap and <link rel=canonical> identical.
  trailingSlash: 'never',
  integrations: [sitemap()],
  // One page, one stylesheet: inlining it removes the render-blocking request.
  build: { inlineStylesheets: 'always' },
  vite: {
    plugins: [tailwindcss()],
  },
})
