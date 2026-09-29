#!/usr/bin/env node
const fs = require("fs");
const zlib = require("zlib");
const { execFileSync } = require("child_process");

const [db, jewelryFile, outFile] = process.argv.slice(2);
if (!db || !jewelryFile || !outFile) {
  console.error("usage: mine_jewelry_rolls.js DB JEWELRY_JSON OUT_JSON");
  process.exit(2);
}

const jewelry = JSON.parse(fs.readFileSync(jewelryFile, "utf8"));
const jewelryIds = new Set(Object.keys(jewelry));

function q(sql, maxBuffer = 512 * 1024 * 1024) {
  return execFileSync("sqlite3", ["-separator", "\t", db, sql], {
    encoding: "utf8",
    maxBuffer,
  });
}

function decode(hex) {
  const buf = Buffer.from(hex || "", "hex");
  for (const fn of [(x) => x, zlib.gunzipSync, zlib.inflateSync]) {
    try {
      return fn(buf).toString("utf8");
    } catch (_) {}
  }
  return "";
}

function safeJson(s) {
  try {
    return JSON.parse(s);
  } catch (_) {
    return null;
  }
}

function propSig(props) {
  props = props || {};
  const one = (k) => (props[k] || []).map((p) => `${p.id}:${p.tier}`).sort();
  return { enchanting: one("ENCHANTING"), grading: one("GRADING") };
}

function itemSig(item) {
  const props = propSig(item.properties);
  return {
    template: item.itemTemplateId,
    name: jewelry[item.itemTemplateId]?.name || null,
    type: jewelry[item.itemTemplateId]?.type_name || null,
    grade: item.grade ?? null,
    arcaneTier: item.arcaneTier ?? null,
    enchanting: props.enchanting,
    grading: props.grading,
    temperingLevel: item.temperingLevel ?? null,
    durability: item.durability ?? null,
  };
}

function findJewelry(node, out = []) {
  if (!node || typeof node !== "object") return out;
  if (Array.isArray(node)) {
    for (const x of node) findJewelry(x, out);
    return out;
  }
  if (typeof node.itemTemplateId === "string" && jewelryIds.has(node.itemTemplateId)) {
    out.push(node);
  }
  for (const v of Object.values(node)) findJewelry(v, out);
  return out;
}

function rollKey(item) {
  return JSON.stringify({
    template: item.template,
    grade: item.grade,
    arcaneTier: item.arcaneTier,
    enchanting: item.enchanting,
    grading: item.grading,
  });
}

function summarize(records, groupKey) {
  const out = {};
  for (const r of records) {
    const k = groupKey(r);
    const b =
      out[k] ||
      (out[k] = {
        key: k,
        template: r.item.template,
        name: r.item.name,
        type: r.item.type,
        samples: 0,
        grades: {},
        arcaneTiers: {},
        enchantCounts: {},
        gradingCounts: {},
        enchantings: {},
        gradings: {},
        rolls: {},
        captureIds: [],
      });
    b.samples++;
    b.captureIds.push(r.id);
    for (const [field, value] of [
      ["grades", r.item.grade],
      ["arcaneTiers", r.item.arcaneTier],
      ["enchantCounts", r.item.enchanting.length],
      ["gradingCounts", r.item.grading.length],
    ]) {
      const kk = String(value);
      b[field][kk] = (b[field][kk] || 0) + 1;
    }
    const e = r.item.enchanting.join("|");
    const g = r.item.grading.join("|");
    const full = rollKey(r.item);
    b.enchantings[e] = (b.enchantings[e] || 0) + 1;
    b.gradings[g] = (b.gradings[g] || 0) + 1;
    b.rolls[full] = (b.rolls[full] || 0) + 1;
  }
  for (const b of Object.values(out)) b.distinctRolls = Object.keys(b.rolls).length;
  return out;
}

function captureRows(where) {
  const sql = `select id, timestamp, url, hex(request_body), hex(response_body) from api_captures where ${where} order by id`;
  const raw = q(sql).trim();
  if (!raw) return [];
  return raw.split("\n").map((line) => {
    const [id, timestamp, url, reqHex, respHex] = line.split("\t");
    return {
      id: Number(id),
      timestamp,
      url,
      request: safeJson(decode(reqHex)),
      response: safeJson(decode(respHex)),
    };
  });
}

const globalRows = captureRows(
  "method='POST' and url like '%globalshops/current/purchase%' and response_status=200",
);
const global = [];
for (const row of globalRows) {
  const product = row.request?.globalShopProductId;
  for (const it of findJewelry(row.response?.reward || row.response)) {
    global.push({ id: row.id, timestamp: row.timestamp, product, item: itemSig(it) });
  }
}

const shopRows = captureRows(
  "method='POST' and url like '%/shops/%' and response_status=200",
);
const opens = [];
const purchases = [];
for (const row of shopRows) {
  if (row.url.endsWith("/purchase")) purchases.push(row);
  else opens.push(row);
}

const catalogsByShop = {};
for (const row of opens) {
  const m = row.url.match(/\/shops\/([0-9a-f-]{36})(?:$|\?)/);
  const shopId = m?.[1];
  const catalog = row.response?.catalog || row.response?.social?.catalog;
  if (!shopId || !catalog) continue;
  catalogsByShop[shopId] = catalogsByShop[shopId] || [];
  catalogsByShop[shopId].push({
    id: row.id,
    timestamp: row.timestamp,
    catalogId: catalog.id,
    bundles: catalog.bundles || [],
  });
}

const shopPurchases = [];
for (const row of purchases) {
  const m = row.url.match(/\/shops\/([0-9a-f-]{36})\/purchase/);
  const shopId = m?.[1];
  const bought = new Set((row.request?.bundles || []).map((b) => b.id));
  for (const it of findJewelry(row.response?.inventory || row.response)) {
    shopPurchases.push({
      id: row.id,
      timestamp: row.timestamp,
      shopId,
      bundleIds: [...bought],
      item: itemSig(it),
    });
  }
}

const report = {
  global: {
    purchaseRows: globalRows.length,
    jewelryGrants: global.length,
    byProduct: summarize(global, (r) => r.product),
    byTemplate: summarize(global, (r) => r.item.template),
  },
  shops: {
    openRows: opens.length,
    purchaseRows: purchases.length,
    jewelryPurchases: shopPurchases.length,
    byTemplate: summarize(shopPurchases, (r) => r.item.template),
    purchases: shopPurchases,
    catalogsByShop,
  },
};

fs.writeFileSync(outFile, JSON.stringify(report, null, 2));
console.log(
  JSON.stringify(
    {
      globalPurchaseRows: globalRows.length,
      globalJewelryGrants: global.length,
      globalProducts: Object.keys(report.global.byProduct).length,
      shopOpenRows: opens.length,
      shopPurchaseRows: purchases.length,
      shopJewelryPurchases: shopPurchases.length,
      shopTemplates: Object.keys(report.shops.byTemplate).length,
    },
    null,
    2,
  ),
);

for (const b of Object.values(report.global.byProduct)
  .filter((b) => b.samples >= 2)
  .sort((a, b) => b.samples - a.samples)
  .slice(0, 20)) {
  console.log("global", b.key, b.name, "samples", b.samples, "distinct", b.distinctRolls);
}
for (const b of Object.values(report.shops.byTemplate).sort((a, b) => b.samples - a.samples)) {
  console.log("shop", b.template, b.name, "samples", b.samples, "distinct", b.distinctRolls);
}
