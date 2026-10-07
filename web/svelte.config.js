import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/vite-plugin-svelte').SvelteConfig} */
export default {
  preprocess: vitePreprocess(),
  compilerOptions: {
    // Our components must use Svelte 5 runes; legacy (Svelte 4) syntax is a
    // compile error. Third-party packages keep whichever mode they were written in.
    runes: ({ filename }) => (filename.split(/[/\\]/).includes('node_modules') ? undefined : true),
  },
};
