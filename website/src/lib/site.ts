export const SITE = {
  name: 'soquel',
  url: 'https://soquel.dev',
  repo: 'https://github.com/soquel-dev/soquel',
  author: 'Joris Gallot',
  licence: 'https://spdx.org/licenses/FSL-1.1-MIT',
  tagline: 'a database client that lends agents the query, not the credentials',
  description:
    'A desktop database client for Postgres, MySQL, SQLite, Redis and MongoDB. Coding agents reach your data through the app over MCP: read-only by default, every write stops for your approval, and credentials never leave the Rust core.',
} as const

/// Self-hosted, cookieless, production only. The privacy page says as much.
export const ANALYTICS = {
  src: 'https://analytics.jorisgallot.dev/j.js',
  websiteId: 'b7a0fa9f-be4b-41eb-88eb-4be7db1dc3c4',
} as const

/// Set a url and the button goes live: the section reads as pre-release for as
/// long as they are all null, so the page is honest in both states.
/// Formats follow the packaging plan: universal dmg, NSIS installer, AppImage
/// (the only Linux format an updater can replace) and a deb that cannot.
export const DOWNLOADS = [
  {
    platform: 'macOS',
    detail: 'Universal, Apple Silicon and Intel',
    format: 'dmg',
    event: 'download-macos',
    url: null as string | null,
  },
  {
    platform: 'Windows',
    detail: 'Installer, x64',
    format: 'exe',
    event: 'download-windows',
    url: null as string | null,
  },
  {
    platform: 'Linux',
    detail: 'AppImage, updates itself',
    format: 'AppImage',
    event: 'download-linux-appimage',
    url: null as string | null,
  },
]

/// Packaged for convenience, but the updater cannot replace a deb: it says so.
export const DEB = {
  event: 'download-linux-deb',
  url: null as string | null,
}

/// Read from here by both /pricing and /terms so a figure cannot drift between the
/// page that quotes it and the page that commits to it.
export const PRICE = {
  licence: '$69',
  renewal: '$35',
  currency: 'USD',
  months: 12,
  activations: 5,
  freeTabs: 'two',
} as const

/// Set the url and the button goes live, same rule as DOWNLOADS: until then the page
/// says it is not on sale, because taking money for something nobody can install is
/// worse than saying "not yet".
export const CHECKOUT = {
  url: null as string | null,
  event: 'checkout',
}

/// Set a src and the shot takes over. Dark theme only: a dark capture reads on
/// either page theme, and light and dark pairs double the upkeep for little.
/// Market seed only, never a real database, or hostnames and connection names
/// ship to a public page.
export const HERO_SHOT = {
  src: null as string | null,
  alt: 'Soquel with an agent asking to run a write, over the table browser',
  spec: 'Table browser with the approval dialog open, market seed, dark, 1440x900',
}

export const SHOTS = [
  {
    src: null as string | null,
    alt: 'The soquel SQL editor showing a query plan as a tree',
    caption: 'The SQL editor renders EXPLAIN as a tree, not a wall of text.',
    spec: 'SQL editor + EXPLAIN tree, market seed, dark, 1440x900',
  },
  {
    src: null as string | null,
    alt: 'The soquel MCP panel with its audit log',
    caption: 'Every agent call, its statement and its outcome, readable in the app.',
    spec: 'MCP panel + audit log, market seed, dark, 1440x900',
  },
]

export const ENGINES = [
  { name: 'PostgreSQL', note: '14 and up' },
  { name: 'MySQL, MariaDB', note: 'MySQL 8.0 and up, MariaDB LTS' },
  { name: 'SQLite', note: 'a file, no server' },
  { name: 'Redis, Valkey', note: 'key browser and console' },
  { name: 'MongoDB', note: 'document browser and console' },
] as const

export const FEATURES = [
  'Table browser with inline editing, filters and export',
  'SQL editor with query plans, rendered as a tree',
  'SSH tunnels with key, agent or password auth, and host key verification',
  'TLS, custom root certificates included',
  'Passwords from the OS keychain, asked at connect, or read from a command',
  'Connections exported to a file, passwords left out unless you encrypt them in',
] as const
