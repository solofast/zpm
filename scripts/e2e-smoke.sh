#!/usr/bin/env bash
# zpm E2E 冒烟：repo create → framework load → project create → db seed → serve → HTTP 断言 → 优雅关闭
set -euo pipefail
ZPM="${ZPM_BIN:-$PWD/target/debug/zpm}"
PORT="${E2E_PORT:-4655}"
T=$(mktemp -d "${TMPDIR:-/tmp}/zpm-e2e.XXXX")
SRV=""
cleanup() {
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null || true
  [ -n "$SRV" ] && wait "$SRV" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT

# 全程隔离：全局配置（含 repo_home 之外的一切状态）指向临时目录
export ZPM_CONFIG_HOME="$T/cfg"

echo "[e2e] 1/5 造 fixture 框架 + repo create smoke"
FW="$T/fw-smoke"
mkdir -p "$FW/knowledge/rules" "$FW/knowledge/lessons" "$FW/knowledge/assets" "$FW/pages"
cat > "$FW/framework.yaml" <<'YAML'
name: smoke-fw
version: 0.1.0
description: e2e 冒烟框架
YAML
cat > "$FW/zpm.config.yaml.tmpl" <<'YAML'
name: {{name}}
server:
  port: {{port}}
  host: 0.0.0.0
  theme: default
datasource:
  type: sqlite
  file: data/source.db
target:
  type: sqlite
  file: data/app.db
sync:
  tables: []
write:
  enabled: false
  mode: simulated
  tables: []
YAML
echo '<!doctype html><html lang="zh"><body>smoke index</body></html>' > "$FW/pages/index.html"
echo '<!doctype html><html lang="zh"><body>workbench</body></html>' > "$FW/pages/workbench.html"
echo '规则索引' > "$FW/knowledge/rules/README.md"

# repo_home 也必须隔离到临时目录：否则元数据库 ~/.zpm/db/zpm.db 会被登记污染
"$ZPM" config set repo_home "$T/zpm-home"
"$ZPM" repo create smoke
"$ZPM" repo switch smoke

echo "[e2e] 2/5 framework load + project create"
"$ZPM" framework load "$FW"
"$ZPM" project create "$T/app" --framework smoke-fw --name demo

cd "$T/app"
echo "[e2e] 3/5 db init + seed"
"$ZPM" db init
cat > seed.sql <<'SQL'
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
INSERT INTO users VALUES (1, 'a'), (2, 'b');
SQL
"$ZPM" db seed seed.sql

echo "[e2e] 4/5 serve on :$PORT"
"$ZPM" serve --port "$PORT" &
SRV=$!
ok=""
for _ in $(seq 1 20); do
  curl -sf --max-time 1 "http://127.0.0.1:$PORT/api/_meta" > /dev/null 2>&1 && { ok=1; break; }
  sleep 0.5
done
[ -n "$ok" ] || { echo "FAIL: 服务未就绪"; exit 1; }

echo "[e2e] 5/5 HTTP 断言"
meta=$(curl -sf "http://127.0.0.1:$PORT/api/_meta")
echo "$meta" | grep -q '"users"'                    || { echo "FAIL: _meta 缺 users 表"; exit 1; }
# fixture 模板 write.enabled=false → 契约值 disabled（写关闭时 mode 不外显）
echo "$meta" | grep -q '"writeMode":"disabled"'     || { echo "FAIL: writeMode 不是 disabled"; exit 1; }
users=$(curl -sf "http://127.0.0.1:$PORT/api/users")
echo "$users" | grep -q '"total":2'                 || { echo "FAIL: users total != 2"; exit 1; }
# 写关闭 → POST 403 table not writable（契约）
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "http://127.0.0.1:$PORT/api/users" \
  -H 'content-type: application/json' -d '{"id":9,"name":"x"}')
[ "$code" = "403" ] || { echo "FAIL: POST 状态码 $code != 403"; exit 1; }
curl -sf -o /dev/null "http://127.0.0.1:$PORT/theme.css" || { echo "FAIL: theme.css"; exit 1; }
curl -sf -o /dev/null "http://127.0.0.1:$PORT/index.html" || { echo "FAIL: index.html"; exit 1; }
# workbench.html（fixture pages/ 自带）
curl -sf -o /dev/null "http://127.0.0.1:$PORT/workbench.html" || { echo "FAIL: workbench.html"; exit 1; }

echo "E2E SMOKE PASS ✓"
