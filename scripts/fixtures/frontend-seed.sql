-- Fixture data for scripts/test-frontend.sh.
--
-- Applied after the server has started, so the schema already exists. That
-- ordering also keeps the run deterministic: the version-check scheduler runs
-- one pass at boot before it sleeps, and at that point this table is empty, so
-- it has nothing to rewrite. The script sets a long VERSION_CHECK_INTERVAL so
-- the next pass never lands inside the run.
--
-- The package list deliberately covers every status badge and the awkward
-- names: `2048.c` and `python-3.11` look like filenames, `aewm++` contains the
-- character that is literal in a path but a space in a query value.

DELETE FROM builds;
DELETE FROM packages;

INSERT INTO packages (name, status, out_of_date, upstream_version, build_flags, platforms, source_type, source_data, directly_requested) VALUES
  ('hello',                  1, 0, '2.12.1-1',  '', 'x86_64', 'aur', '{"type":"aur","name":"hello"}',                  1),
  ('neofetch',               1, 1, '7.1.0-2',   '', 'x86_64', 'aur', '{"type":"aur","name":"neofetch"}',               1),
  ('yay',                    0, 0, '12.4.2-1',  '', 'x86_64', 'aur', '{"type":"aur","name":"yay"}',                    1),
  ('paru',                   2, 0, '2.0.4-1',   '', 'x86_64', 'aur', '{"type":"aur","name":"paru"}',                   1),
  ('visual-studio-code-bin', 3, 0, '1.92.0-1',  '', 'x86_64', 'aur', '{"type":"aur","name":"visual-studio-code-bin"}', 1),
  ('2048.c',                 4, 0, '1.0-3',     '', 'x86_64', 'aur', '{"type":"aur","name":"2048.c"}',                 1),
  ('aewm++',                 1, 0, '1.1.6-4',   '', 'x86_64', 'aur', '{"type":"aur","name":"aewm++"}',                 1),
  ('python-3.11',            1, 0, '3.11.9-1',  '', 'x86_64', 'aur', '{"type":"aur","name":"python-3.11"}',            1),
  -- Deliberately gets no build below, and no upstream version either: this is
  -- a package that was just promoted from a dependency and has not been
  -- version-checked yet. Both "no version yet" placeholders end up on screen
  -- rather than only in a unit test.
  ('never-built',            3, 0, NULL,        '', 'x86_64', 'aur', '{"type":"aur","name":"never-built"}',            1);

-- Timestamps are Unix *seconds*: `aurcache-api/src/stats.rs` compares
-- start_time against strftime('%s', 'now'). Milliseconds render as dates
-- decades in the future.
--
-- The spread exercises every branch of the duration and age formatters,
-- including a build that started but never finished (NULL end_time), which
-- must read as unknown rather than as a zero-length build.
INSERT INTO builds (pkg_id, output, status, start_time, end_time, platform, version)
SELECT
  p.id,
  '==> Making package: ' || p.name || char(10) || '==> Retrieving sources...',
  p.status,
  CAST(strftime('%s','now') AS INTEGER) - offs.age,
  CASE WHEN offs.dur IS NULL THEN NULL
       ELSE CAST(strftime('%s','now') AS INTEGER) - offs.age + offs.dur END,
  'x86_64',
  p.upstream_version
FROM packages p
JOIN (
  SELECT 'hello' AS name,                  120    AS age, 43   AS dur UNION ALL
  SELECT 'neofetch',                       900,           187         UNION ALL
  SELECT 'yay',                            18000,         3720        UNION ALL
  SELECT 'paru',                           172800,        61          UNION ALL
  SELECT 'visual-studio-code-bin',         300,           NULL        UNION ALL
  SELECT '2048.c',                         777600,        7860        UNION ALL
  SELECT 'aewm++',                         45,            12          UNION ALL
  SELECT 'python-3.11',                    93600,         900
) offs ON offs.name = p.name;

UPDATE packages
SET latest_build = (SELECT b.id FROM builds b WHERE b.pkg_id = packages.id ORDER BY b.id DESC LIMIT 1);
