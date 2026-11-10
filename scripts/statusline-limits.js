// Limites da conta do Claude para o Process Monitor (tela "IA").
//
// O Claude Code entrega ao script da statusline, a cada atualização, o `rate_limits` da
// conta (`five_hour`, `seven_day`, com `used_percentage` e `resets_at`). Este módulo grava
// só isso, sem nenhum segredo, em %LOCALAPPDATA%\Process Monitor\claude-limits.json, que
// o Process Monitor lê (src/usage/bridge.rs). O formato é contrato entre os dois:
//
//   {"version":1, "written_at":<ms>, "rate_limits":{"five_hour":{"used_percentage":62,
//    "resets_at":<o que o Claude Code mandar>}, "seven_day":{...}}}
//
// Instalação: copie para ~/.claude/statusline-limits.js e, no ~/.claude/statusline.js,
// dentro do process.stdin.on('end', ...), logo depois do JSON.parse:
//
//   try { require(path.join(__dirname, 'statusline-limits.js'))(d.rate_limits); } catch (e) {}
//
// Qualquer erro aqui é engolido: a statusline continua igual.
const fs = require('fs');
const path = require('path');

/** Milissegundos de um `resets_at` em segundos, milissegundos ou texto ISO. */
function toMs(v) {
  if (typeof v === 'number') return v > 1e11 ? v : v * 1000;
  if (typeof v === 'string') {
    const n = Number(v);
    if (!Number.isNaN(n)) return toMs(n);
    const t = Date.parse(v);
    return Number.isNaN(t) ? null : t;
  }
  return null;
}

module.exports = function saveLimits(rateLimits) {
  try {
    if (!rateLimits || typeof rateLimits !== 'object' || !process.env.LOCALAPPDATA) return;
    const dir = path.join(process.env.LOCALAPPDATA, 'Process Monitor');
    const file = path.join(dir, 'claude-limits.json');
    let old = {};
    try {
      old = JSON.parse(fs.readFileSync(file, 'utf8').replace(/^﻿/, '')).rate_limits || {};
    } catch (e) {}
    const out = {};
    for (const [kind, w] of Object.entries(rateLimits)) {
      if (!w || typeof w.used_percentage !== 'number') continue;
      const entry = { used_percentage: w.used_percentage, resets_at: w.resets_at ?? null };
      const prev = old[kind];
      if (prev && typeof prev.used_percentage === 'number') {
        const a = toMs(prev.resets_at);
        const b = toMs(entry.resets_at);
        // Outra sessão aberta pode mandar uma leitura mais velha: de uma janela anterior
        // (fica a mais nova) ou da mesma janela (dentro dela o uso só cresce).
        if (a !== null && b !== null && a > b + 60000) {
          out[kind] = prev;
          continue;
        }
        if (a !== null && b !== null && Math.abs(a - b) <= 60000 && prev.used_percentage > entry.used_percentage) {
          entry.used_percentage = prev.used_percentage;
        }
      }
      out[kind] = entry;
    }
    if (!Object.keys(out).length) return;
    fs.mkdirSync(dir, { recursive: true });
    const tmp = `${file}.${process.pid}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify({ version: 1, written_at: Date.now(), rate_limits: out }));
    fs.renameSync(tmp, file);
  } catch (e) {}
};
