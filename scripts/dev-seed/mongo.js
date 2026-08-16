/* global db, ObjectId, NumberDecimal, UUID */
// Dev seed: SaaS-shaped documents mirroring the sql dev seeds. Re-runnable:
// drops the target database first.
//   users      5 000  nested profiles, plans, signup dates, unique email index
//   orders    20 000  user refs, Decimal128 amounts, {userId, createdAt} index
//   events    50 000  heterogeneous shapes per type, {type, at} index
//   sessions  10 000  UUID tokens (BinData) for the $binary rendering
//   webhooks      25
const env = globalThis.process?.env ?? {}
const name = env.SEED_DATABASE || 'soquel_dev'
const dev = db.getSiblingDB(name)
dev.dropDatabase()

const DAY = 86_400_000
// Every date hangs off this instead of the wall clock, so re-seeding with the
// same base date reproduces the same documents (website screenshots).
const baseEpoch = Number(env.SEED_BASE_EPOCH)
const base = baseEpoch > 0 ? baseEpoch * 1000 : Date.now()
const start = base - 560 * DAY

const plans = ['free', 'pro', 'team', 'enterprise']
const cities = ['Lyon', 'Paris', 'Nantes', 'Berlin', 'Madrid', 'Austin']
const firsts = [
  'Alice', 'Marcus', 'Priya', 'Tomas', 'Chloe', 'Daniel', 'Sofia', 'Omar', 'Hannah', 'Lucas',
  'Nadia', 'Felix', 'Clara', 'Victor', 'Amina', 'Jonas', 'Elena', 'Mateo', 'Iris', 'Samuel',
]
const lasts = [
  'Bennett', 'Novak', 'Iyer', 'Lindqvist', 'Moreau', 'Okafor', 'Ferrari', 'Haddad', 'Weber',
  'Silva', 'Kovacs', 'Duarte', 'Larsen', 'Nakamura', 'Fischer', 'Almeida', 'Whitfield',
]
const brands = [
  'northwind', 'lakeside', 'meridian', 'brightline', 'harborview', 'crestline', 'evergreen',
  'stonebridge', 'westport', 'ironwood', 'fernwood', 'clearwater', 'ridgeway', 'copperfield',
  'bayside', 'silverlake', 'highfield', 'oakmont', 'redstone', 'thornbury',
]
const tlds = ['io', 'com', 'dev']

// The two lists are walked by a stride coprime with their 340 pairs: both names
// move on every document, and a pair only comes back every 340, which the
// numeric suffix on the email then disambiguates.
const PAIRS = firsts.length * lasts.length
const personName = (i) => {
  const m = (i * 21) % PAIRS
  return `${firsts[m % firsts.length]} ${lasts[Math.floor(m / firsts.length) % lasts.length]}`
}
const personEmail = (i) => {
  const local = personName(i).toLowerCase().replace(' ', '.')
  const seen = Math.floor(i / PAIRS)
  return `${local}${seen === 0 ? '' : seen}@${brands[i % brands.length]}.${tlds[i % tlds.length]}`
}

function batchInsert(collection, total, make) {
  const batch = []
  for (let i = 0; i < total; i++) {
    batch.push(make(i))
    if (batch.length === 2000) {
      collection.insertMany(batch)
      batch.length = 0
    }
  }
  if (batch.length > 0)
    collection.insertMany(batch)
}

const userIds = []
batchInsert(dev.users, 5000, (i) => {
  const _id = new ObjectId()
  userIds.push(_id)
  return {
    _id,
    email: personEmail(i),
    name: personName(i),
    plan: plans[i % plans.length],
    profile: { city: cities[i % cities.length], timezone: 'Europe/Paris', logins: i % 400 },
    tags: i % 7 === 0 ? ['beta', 'newsletter'] : ['newsletter'],
    createdAt: new Date(start + (i % 500) * DAY),
  }
})
dev.users.createIndex({ email: 1 }, { unique: true })

const statuses = ['pending', 'paid', 'shipped', 'refunded']
batchInsert(dev.orders, 20000, i => ({
  userId: userIds[i % userIds.length],
  status: statuses[i % statuses.length],
  amount: NumberDecimal(((i % 900) * 13.37 / 100).toFixed(2)),
  items: (i % 3) + 1,
  createdAt: new Date(start + (i % 550) * DAY),
}))
dev.orders.createIndex({ userId: 1, createdAt: -1 })

const types = ['page_view', 'api_call', 'export', 'login']
const TOTAL_EVENTS = 50000
batchInsert(dev.events, TOTAL_EVENTS, (i) => {
  const type = types[i % types.length]
  // Ends at the base date: an event log reads as recent activity.
  const common = { type, at: new Date(base - (TOTAL_EVENTS - i) * 60_000), userId: userIds[i % userIds.length] }
  if (type === 'page_view')
    return { ...common, path: `/app/page-${i % 40}` }
  if (type === 'api_call')
    return { ...common, endpoint: `/v1/resource/${i % 12}`, ms: i % 900 }
  if (type === 'export')
    return { ...common, rows: (i % 100) * 1000, format: i % 2 === 0 ? 'csv' : 'json' }
  return { ...common, ip: `10.0.${i % 255}.${(i * 7) % 255}` }
})
dev.events.createIndex({ type: 1, at: -1 })

batchInsert(dev.sessions, 10000, i => ({
  userId: userIds[i % userIds.length],
  token: UUID(),
  ua: i % 3 === 0 ? 'Mozilla/5.0 (Macintosh)' : 'Mozilla/5.0 (Windows NT 10.0)',
  expiresAt: new Date(base + (i % 30) * DAY),
}))

const hooks = ['order.paid', 'user.created', 'invoice.sent', 'export.ready']
batchInsert(dev.webhooks, 25, i => ({
  url: `https://hooks.${brands[i % brands.length]}.${tlds[i % tlds.length]}/soquel`,
  events: i % 2 === 0 ? [hooks[i % hooks.length]] : [hooks[i % hooks.length], 'user.created'],
  active: i % 5 !== 0,
  secretHash: `sha256:${i.toString(16).padStart(8, '0')}`,
}))

// A user the app connects as, so a screenshot shows an application user rather
// than the container's root. Users live in admin, so dropDatabase() leaves it.
if (dev.getUser('api'))
  dev.dropUser('api')
dev.createUser({ user: 'api', pwd: 'api', roles: [{ role: 'readWrite', db: name }] })

print(`${name}: ${dev.users.estimatedDocumentCount()} users, ${dev.orders.estimatedDocumentCount()} orders, ${dev.events.estimatedDocumentCount()} events, ${dev.sessions.estimatedDocumentCount()} sessions, ${dev.webhooks.estimatedDocumentCount()} webhooks`)
