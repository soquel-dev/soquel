import type { APIRoute } from 'astro'
import { SITE } from '@/lib/site'

// Explicit groups: a UA-specific group makes a crawler ignore `*`, so the allow
// has to be restated. Answer engines are welcome here, the people who would use
// this tool ask an assistant before they ask a search box.
const AI_AGENTS = [
  'GPTBot',
  'OAI-SearchBot',
  'ChatGPT-User',
  'ClaudeBot',
  'Claude-User',
  'Claude-SearchBot',
  'anthropic-ai',
  'PerplexityBot',
  'Perplexity-User',
  'Google-Extended',
  'Applebot-Extended',
  'meta-externalagent',
  'Amazonbot',
  'DuckAssistBot',
  'cohere-ai',
  'YouBot',
]

export const GET: APIRoute = ({ site }) => {
  const origin = (site ?? new URL(SITE.url)).origin

  const body = `User-agent: *
Allow: /

${AI_AGENTS.map(ua => `User-agent: ${ua}`).join('\n')}
Allow: /

# LLM-readable summaries: ${origin}/llms.txt and ${origin}/llms-full.txt
Sitemap: ${origin}/sitemap-index.xml
`

  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } })
}
