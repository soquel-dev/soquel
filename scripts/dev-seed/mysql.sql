-- Dev seed for mysql, mirroring the postgres SaaS shape. Re-runnable.
--   organizations   2 000
--   users         100 000
--   projects       10 000
--   tasks         300 000
--   invoices      100 000
--   events      1 000 000

-- Every timestamp hangs off this instead of the wall clock, so re-seeding with
-- the same base date reproduces the same rows (website screenshots). Set it
-- before this file to pin it; seed.sh does.
SET @base_date = CAST(IFNULL(NULLIF(@base_date, ''), NOW()) AS DATETIME);

SET FOREIGN_KEY_CHECKS = 0;
DROP TABLE IF EXISTS events, invoices, tasks, projects, users, organizations, seq_digits, seq;
DROP VIEW IF EXISTS active_projects;
SET FOREIGN_KEY_CHECKS = 1;

-- 1..1_000_000 via digit cross joins: no recursion depth limits involved.
CREATE TABLE seq_digits (d INT NOT NULL);
INSERT INTO seq_digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
CREATE TABLE seq (n INT NOT NULL PRIMARY KEY);
INSERT INTO seq (n)
SELECT 1 + d1.d + 10*d2.d + 100*d3.d + 1000*d4.d + 10000*d5.d + 100000*d6.d
FROM seq_digits d1, seq_digits d2, seq_digits d3, seq_digits d4, seq_digits d5, seq_digits d6;

CREATE TABLE organizations (
  id INT AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(255) NOT NULL,
  plan VARCHAR(16) NOT NULL,
  seats INT NOT NULL,
  metadata JSON,
  created_at DATETIME NOT NULL
);

-- 40 brands x 8 suffixes walked by a stride coprime with their 320 pairs: both
-- words move on every row (a shared suffix down a whole screen is what reads as
-- generated) and no pair repeats before 320.
INSERT INTO organizations (name, plan, seats, metadata, created_at)
SELECT
  CONCAT(
    ELT(1 + m % 40,
      'Northwind','Lakeside','Meridian','Brightline','Harborview','Crestline','Evergreen','Stonebridge','Westport','Ironwood',
      'Fernwood','Clearwater','Ridgeway','Copperfield','Bayside','Silverlake','Highfield','Oakmont','Redstone','Thornbury',
      'Kingsford','Ashford','Elmgrove','Fairview','Glenmore','Havenwood','Lockridge','Marbury','Norbrook','Oakhurst',
      'Pinehurst','Queensbury','Rosemont','Southgate','Templeton','Underhill','Vinewood','Westbrook','Yarrow','Zephyr'), ' ',
    ELT(1 + (m DIV 40) % 8, 'Labs','Group','Systems','Digital','Studio','Works','Industries','Partners')
  ),
  ELT(1 + n % 4, 'free','pro','business','enterprise'),
  5 + n % 200,
  CASE WHEN n % 5 = 0 THEN NULL
       ELSE JSON_OBJECT('industry', ELT(1 + n % 5, 'fintech','health','retail','media','gaming'), 'mrr', (n % 900) * 10) END,
  @base_date - INTERVAL (n % 1000) DAY
FROM (SELECT n, (n * 41) % 320 AS m FROM seq WHERE n <= 2000) s;

CREATE TABLE users (
  id INT AUTO_INCREMENT PRIMARY KEY,
  organization_id INT NOT NULL,
  email VARCHAR(255) NOT NULL UNIQUE,
  name VARCHAR(128) NOT NULL,
  role VARCHAR(16) NOT NULL,
  active BOOLEAN NOT NULL,
  tags JSON NOT NULL,
  prefs JSON,
  last_login_at DATETIME,
  created_at DATETIME NOT NULL,
  CONSTRAINT users_org_fk FOREIGN KEY (organization_id) REFERENCES organizations (id)
);

-- 20 first names x 17 last names walked by a stride coprime with their 340
-- pairs: both names move on every row, and a pair only comes back every 340,
-- which the numeric suffix on the email then disambiguates (the unique index
-- holds because the stride is a bijection modulo 340). The domain is the row's
-- own organization, so the two columns agree.
INSERT INTO users (organization_id, email, name, role, active, tags, prefs, last_login_at, created_at)
SELECT
  1 + n % 2000,
  CONCAT(
    LOWER(ELT(1 + m % 20,
      'Alice','Marcus','Priya','Tomas','Chloe','Daniel','Sofia','Omar','Hannah','Lucas',
      'Nadia','Felix','Clara','Victor','Amina','Jonas','Elena','Mateo','Iris','Samuel')), '.',
    LOWER(ELT(1 + (m DIV 20) % 17,
      'Bennett','Novak','Iyer','Lindqvist','Moreau','Okafor','Ferrari','Haddad','Weber',
      'Silva','Kovacs','Duarte','Larsen','Nakamura','Fischer','Almeida','Whitfield')),
    IF(n DIV 340 = 0, '', n DIV 340),
    '@',
    LOWER(ELT(1 + (1 + n % 2000) % 40,
      'Northwind','Lakeside','Meridian','Brightline','Harborview','Crestline','Evergreen','Stonebridge','Westport','Ironwood',
      'Fernwood','Clearwater','Ridgeway','Copperfield','Bayside','Silverlake','Highfield','Oakmont','Redstone','Thornbury',
      'Kingsford','Ashford','Elmgrove','Fairview','Glenmore','Havenwood','Lockridge','Marbury','Norbrook','Oakhurst',
      'Pinehurst','Queensbury','Rosemont','Southgate','Templeton','Underhill','Vinewood','Westbrook','Yarrow','Zephyr')), '.',
    ELT(1 + (1 + n % 2000) % 3, 'io','com','dev')
  ),
  CONCAT(
    ELT(1 + m % 20,
      'Alice','Marcus','Priya','Tomas','Chloe','Daniel','Sofia','Omar','Hannah','Lucas',
      'Nadia','Felix','Clara','Victor','Amina','Jonas','Elena','Mateo','Iris','Samuel'), ' ',
    ELT(1 + (m DIV 20) % 17,
      'Bennett','Novak','Iyer','Lindqvist','Moreau','Okafor','Ferrari','Haddad','Weber',
      'Silva','Kovacs','Duarte','Larsen','Nakamura','Fischer','Almeida','Whitfield')
  ),
  ELT(1 + n % 4, 'owner','admin','member','viewer'),
  n % 7 <> 0,
  CASE n % 3 WHEN 0 THEN JSON_ARRAY('beta') WHEN 1 THEN JSON_ARRAY('vip','beta') ELSE JSON_ARRAY() END,
  CASE WHEN n % 4 = 0 THEN NULL
       ELSE JSON_OBJECT('theme', IF(n % 2 = 0, 'dark', 'light'), 'locale', ELT(1 + n % 4, 'en','fr','de','es')) END,
  CASE WHEN n % 11 = 0 THEN NULL ELSE @base_date - INTERVAL (n % 90) HOUR END,
  @base_date - INTERVAL (n % 700) DAY
FROM (SELECT n, (n * 21) % 340 AS m FROM seq WHERE n <= 100000) s;

CREATE TABLE projects (
  id INT AUTO_INCREMENT PRIMARY KEY,
  organization_id INT NOT NULL,
  name VARCHAR(128) NOT NULL,
  status VARCHAR(16) NOT NULL,
  budget DECIMAL(12, 2),
  archived BOOLEAN NOT NULL DEFAULT FALSE,
  created_at DATETIME NOT NULL,
  CONSTRAINT projects_org_fk FOREIGN KEY (organization_id) REFERENCES organizations (id)
);

INSERT INTO projects (organization_id, name, status, budget, archived, created_at)
SELECT
  1 + n % 2000,
  CONCAT(
    ELT(1 + m % 16,
      'Apollo','Hermes','Atlas','Nova','Orion','Vega','Lyra','Draco',
      'Solstice','Beacon','Compass','Lantern','Summit','Trellis','Northstar','Quarry'), ' ',
    ELT(1 + (m DIV 16) % 9, 'Migration','Rollout','Redesign','Platform','Pipeline','Revamp','Launch','Cleanup','Rewrite')
  ),
  ELT(1 + n % 4, 'draft','active','paused','done'),
  CASE WHEN n % 6 = 0 THEN NULL ELSE (n % 5000) * 25 END,
  n % 9 = 0,
  @base_date - INTERVAL (n % 400) DAY
FROM (SELECT n, (n * 17) % 144 AS m FROM seq WHERE n <= 10000) s;

CREATE TABLE tasks (
  id INT AUTO_INCREMENT PRIMARY KEY,
  project_id INT NOT NULL,
  assignee_id INT,
  title VARCHAR(255) NOT NULL,
  state VARCHAR(16) NOT NULL,
  priority INT NOT NULL,
  estimate_hours DECIMAL(6, 2),
  due_date DATE,
  created_at DATETIME NOT NULL,
  CONSTRAINT tasks_project_fk FOREIGN KEY (project_id) REFERENCES projects (id),
  CONSTRAINT tasks_assignee_fk FOREIGN KEY (assignee_id) REFERENCES users (id)
);

INSERT INTO tasks (project_id, assignee_id, title, state, priority, estimate_hours, due_date, created_at)
SELECT
  1 + n % 10000,
  CASE WHEN n % 8 = 0 THEN NULL ELSE 1 + n % 100000 END,
  CONCAT(
    ELT(1 + m % 8, 'Fix','Implement','Review','Design','Refactor','Document','Test','Ship'), ' ',
    ELT(1 + (m DIV 8) % 15,
      'login flow','billing page','api client','search index','onboarding emails',
      'csv exports','webhook retries','dashboard filters','invoice pdf','session cache',
      'audit log','rate limiter','signup form','password reset','usage report')
  ),
  ELT(1 + n % 4, 'todo','in-progress','blocked','done'),
  1 + n % 5,
  CASE WHEN n % 4 = 0 THEN NULL ELSE ((n % 40) + 1) / 2 END,
  CASE WHEN n % 3 = 0 THEN NULL ELSE DATE(@base_date) + INTERVAL (n % 60 - 30) DAY END,
  @base_date - INTERVAL (n % 300) DAY
FROM (SELECT n, (n * 17) % 120 AS m FROM seq WHERE n <= 300000) s;

CREATE TABLE invoices (
  id INT AUTO_INCREMENT PRIMARY KEY,
  organization_id INT NOT NULL,
  number VARCHAR(32) NOT NULL UNIQUE,
  amount DECIMAL(12, 2) NOT NULL,
  currency CHAR(3) NOT NULL,
  paid BOOLEAN NOT NULL,
  issued_at DATETIME NOT NULL,
  pdf BLOB,
  CONSTRAINT invoices_org_fk FOREIGN KEY (organization_id) REFERENCES organizations (id)
);

INSERT INTO invoices (organization_id, number, amount, currency, paid, issued_at, pdf)
SELECT
  1 + n % 2000,
  CONCAT('INV-', YEAR(@base_date - INTERVAL (n % 730) DAY), '-', LPAD(n, 7, '0')),
  ((n % 100000) + 100) / 100 * 12,
  ELT(1 + n % 3, 'EUR','USD','GBP'),
  n % 5 <> 0,
  @base_date - INTERVAL (n % 730) DAY,
  CASE WHEN n % 50 = 0 THEN UNHEX(MD5(n)) ELSE NULL END
FROM seq WHERE n <= 100000;

CREATE TABLE events (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  user_id INT NOT NULL,
  kind VARCHAR(32) NOT NULL,
  payload JSON,
  at DATETIME NOT NULL,
  CONSTRAINT events_user_fk FOREIGN KEY (user_id) REFERENCES users (id),
  INDEX events_at_idx (at)
);

INSERT INTO events (user_id, kind, payload, at)
SELECT
  1 + n % 100000,
  ELT(1 + n % 8, 'page_view','click','signup','purchase','logout','api_call','export','invite'),
  CASE WHEN n % 10 = 0 THEN NULL
       ELSE JSON_OBJECT('path', CONCAT('/', ELT(1 + n % 5, 'home','settings','billing','projects','tasks')), 'ms', n % 1500) END,
  @base_date - INTERVAL (n % 5184000) SECOND
FROM seq;

CREATE VIEW active_projects AS
SELECT p.id, o.name AS organization, p.name, p.status, p.budget,
       COUNT(t.id) AS open_tasks
FROM projects p
JOIN organizations o ON o.id = p.organization_id
LEFT JOIN tasks t ON t.project_id = p.id AND t.state <> 'done'
WHERE NOT p.archived AND p.status = 'active'
GROUP BY p.id, o.name, p.name, p.status, p.budget;

DROP TABLE seq;
DROP TABLE seq_digits;

ANALYZE TABLE organizations, users, projects, tasks, invoices, events;
