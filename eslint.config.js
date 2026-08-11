import antfu from '@antfu/eslint-config'

export default antfu({
  typescript: true,
  // cargo fmt owns Cargo.toml; eslint has no Rust TOML to lint.
  toml: false,
  ignores: [
    '**/dist/**',
    '**/node_modules/**',
    '**/target/**',
    // A mongosh script: its own globals (db, ObjectId), not Node.
    'scripts/dev-seed/mongo.js',
    // Own workspace, own lockfile, own lint and CI job.
    'landing/**',
  ],
})
