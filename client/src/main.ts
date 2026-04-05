import * as flatbuffers from "flatbuffers";
import { WorldSnapshot } from "./generated/world/world-snapshot.js";
import { BountyStatus } from "./generated/world/bounty-status.js";

const ANIM_NAMES = ["Idle", "Walking", "Working"];
const ANIM_CLASSES = ["anim-idle", "anim-walking", "anim-working"];
const BOUNTY_STATES = ["Available", "Claimed", "Completed", "Submitted"];
const BOUNTY_CLASSES = ["bounty-available", "bounty-claimed", "bounty-completed", "bounty-submitted"];

const $ = (s: string) => document.getElementById(s)!;
const connStatus = $("conn-status");
const tickCount = $("tick-count");
const msgRate = $("msg-rate");
const agentCardsEl = $("agent-cards");
const bountiesTable = $("bounties-table");
const structuresTable = $("structures-table");
const queueInfo = $("queue-info");
const chatLog = $("chat-log");
const relsTable = $("rels-table");

// Per-agent history (persisted across renders).
const agentThoughts: Record<string, string[]> = {};
const agentActions: Record<string, { tick: number; text: string }[]> = {};
const MAX_HISTORY = 10;

let messageCount = 0;
setInterval(() => { msgRate.textContent = `${messageCount} msg/s`; messageCount = 0; }, 1000);

function connect() {
  const protocol = location.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(`${protocol}//${location.host}/ws`);
  ws.binaryType = "arraybuffer";
  ws.onopen = () => { connStatus.textContent = "connected"; connStatus.className = "connected"; };
  ws.onclose = () => { connStatus.textContent = "disconnected"; connStatus.className = "disconnected"; setTimeout(connect, 2000); };
  ws.onerror = () => ws.close();
  ws.onmessage = (event) => {
    messageCount++;
    const buf = new flatbuffers.ByteBuffer(new Uint8Array(event.data));
    render(WorldSnapshot.getRootAsWorldSnapshot(buf));
  };
}

function needBar(val: number, label: string): string {
  const pct = Math.round(val);
  const color = pct > 50 ? "#4caf50" : pct > 25 ? "#ff9800" : "#f44336";
  return `<span class="nl">${label}</span><span class="nb"><span class="nf" style="width:${pct}%;background:${color}"></span></span><span class="nv">${pct}</span>`;
}

function itemClass(name: string): string {
  if (name.includes("card") || name.includes("Card")) return "item-card";
  if (name.includes("document") || name.includes("Document") || name.includes(".md")) return "item-doc";
  if (name.includes("egg") || name.includes("Egg")) return "item-egg";
  return "";
}

function render(s: WorldSnapshot) {
  tickCount.textContent = s.tick().toString();
  const tick = Number(s.tick());

  // Collect per-agent logs from the event log.
  for (let i = 0; i < s.eventLogLength(); i++) {
    const entry = s.eventLog(i)!;
    const agent = entry.agent() ?? "?";
    const kind = entry.kind() ?? "";
    const text = entry.text() ?? "";
    const entryTick = Number(entry.tick());

    if (kind === "thought" && text) {
      if (!agentThoughts[agent]) agentThoughts[agent] = [];
      const arr = agentThoughts[agent];
      if (arr.length === 0 || arr[arr.length - 1] !== text) {
        arr.push(text);
        if (arr.length > MAX_HISTORY) arr.shift();
      }
    }
    if ((kind === "action" || kind === "decision") && text) {
      if (!agentActions[agent]) agentActions[agent] = [];
      const arr = agentActions[agent];
      if (arr.length === 0 || arr[arr.length - 1].text !== text) {
        arr.push({ tick: entryTick, text });
        if (arr.length > MAX_HISTORY) arr.shift();
      }
    }
  }

  // Agent cards (sorted by name for stable ordering)
  const agentIndices: number[] = [];
  for (let i = 0; i < s.agentsLength(); i++) agentIndices.push(i);
  agentIndices.sort((a, b) => (s.agents(a)!.name() ?? "").localeCompare(s.agents(b)!.name() ?? ""));

  let cardsHtml = "";
  for (const i of agentIndices) {
    const a = s.agents(i)!;
    const pos = a.pos();
    const anim = a.animation();
    const needs = a.needs();
    const action = a.currentAction();
    const actionTicks = a.actionTicksLeft();
    const agentName = a.name() ?? "?";
    const color = agentColor(agentName);
    const goal = a.goal() ?? "?";

    const needsHtml = needs
      ? `<div class="needs-row">${needBar(needs.energy(), "E")}${needBar(needs.hunger(), "H")}${needBar(needs.boredom(), "B")}</div>`
      : "";

    // Inventory items
    let invHtml = "";
    for (let j = 0; j < a.inventoryLength(); j++) {
      const slot = a.inventory(j);
      if (slot) {
        const itemName = slot.itemType() ?? "?";
        const cls = itemClass(itemName);
        if (itemName.startsWith("doc:")) {
          // Clickable document — fetches and shows in modal
          const docTitle = itemName.substring(4);
          const docUrl = `/api/documents/${agentName.toLowerCase()}/${docTitle}`;
          invHtml += `<span class="item item-doc" style="cursor:pointer" title="Click to read" onclick="fetch('${docUrl}').then(r=>r.text()).then(t=>{const m=document.getElementById('doc-modal')!;document.getElementById('doc-content')!.textContent=t;document.getElementById('doc-title')!.textContent='${docTitle}';m.style.display='flex'})">${docTitle}</span>`;
        } else {
          invHtml += `<span class="item ${cls}">${itemName}${slot.count() > 1 ? ` x${slot.count()}` : ''}</span>`;
        }
      }
    }

    // Thoughts (most recent first)
    const thoughts = agentThoughts[agentName] || [];
    let thoughtsHtml = "";
    for (let t = thoughts.length - 1; t >= Math.max(0, thoughts.length - 5); t--) {
      const text = thoughts[t].length > 120 ? thoughts[t].substring(0, 120) + "..." : thoughts[t];
      thoughtsHtml += `<div class="thought-entry">${text}</div>`;
    }

    // Actions (most recent first)
    const actions = agentActions[agentName] || [];
    let actionsHtml = "";
    for (let t = actions.length - 1; t >= Math.max(0, actions.length - 5); t--) {
      actionsHtml += `<div class="action-entry"><span class="act-tick">${actions[t].tick}</span><span class="act-text">${actions[t].text}</span></div>`;
    }

    const actionHtml = action ? `<span style="color:#ff9800">${action} (${actionTicks}t)</span>` : "";

    // Token tracking stats
    const stats = a.stats();
    const tokensUsed = stats ? stats.tokensUsed() : 0;
    const contextLimit = stats ? stats.contextLimit() : 200000;
    const totalCost = stats ? stats.totalCostUsd() : 0;
    const tokenPct = contextLimit > 0 ? Math.round((tokensUsed / contextLimit) * 100) : 0;
    const costStr = totalCost >= 0.01 ? `$${totalCost.toFixed(2)}` : `$${totalCost.toFixed(4)}`;
    const tokenColor = tokenPct < 50 ? "#4caf50" : tokenPct < 75 ? "#ff9800" : "#f44336";

    cardsHtml += `
      <div class="agent-card">
        <div class="card-header">
          <span class="card-name" style="color:${color}">${agentName}</span>
          <span class="card-gold">${a.gold()}g</span>
        </div>
        <div class="card-meta">
          <span>(${pos?.x()}, ${pos?.y()})</span>
          <span class="${ANIM_CLASSES[anim] ?? ""}">${ANIM_NAMES[anim] ?? "?"}</span>
          ${actionHtml}
        </div>
        <div class="card-meta">
          <span style="color:${tokenColor}">${(tokensUsed / 1000).toFixed(1)}k/${(contextLimit / 1000).toFixed(0)}k tokens (${tokenPct}%)</span>
          <span class="muted">${costStr}</span>
        </div>
        <div class="card-meta"><span class="muted">${goal}</span></div>
        ${needsHtml}
        <div class="card-section">
          <div class="card-section-title">Inventory</div>
          <div class="card-inventory">${invHtml || '<span class="muted">empty</span>'}</div>
        </div>
        <div class="card-section">
          <div class="card-section-title">Latest Thoughts</div>
          <div class="card-thoughts">${thoughtsHtml || '<span class="muted">no thoughts yet</span>'}</div>
        </div>
        <div class="card-section">
          <div class="card-section-title">Recent Actions</div>
          <div class="card-actions">${actionsHtml || '<span class="muted">no actions yet</span>'}</div>
        </div>
      </div>`;
  }
  agentCardsEl.innerHTML = cardsHtml;

  // Active chats (deduplicate)
  const seenConvos = new Set<string>();
  let chatHtml = "";
  for (let i = 0; i < s.agentsLength(); i++) {
    const a = s.agents(i)!;
    if (a.activeChatLength() > 0) {
      const speakers = new Set<string>();
      for (let j = 0; j < a.activeChatLength(); j++) {
        speakers.add(a.activeChat(j)!.speaker() ?? "?");
      }
      speakers.add(a.name() ?? "?");
      const key = [...speakers].sort().join("+");
      if (seenConvos.has(key)) continue;
      seenConvos.add(key);

      chatHtml += `<div class="chat-header" style="color:#888;margin-top:4px"><em>${a.name()}'s conversation:</em></div>`;
      for (let j = 0; j < a.activeChatLength(); j++) {
        const msg = a.activeChat(j)!;
        const speaker = msg.speaker() ?? "?";
        chatHtml += `<div class="chat-msg"><strong style="color:${agentColor(speaker)}">${speaker}</strong>: ${msg.text()}</div>`;
      }
    }
  }
  chatLog.innerHTML = chatHtml || '<span class="muted">No active conversations</span>';

  // Relationships
  let rh = "";
  for (let i = 0; i < s.agentsLength(); i++) {
    const a = s.agents(i)!;
    for (let j = 0; j < a.relationshipsLength(); j++) {
      const rel = a.relationships(j)!;
      rh += `<tr><td>${a.name()}</td><td>${rel.agentName()}</td><td class="gold">${rel.friendship()}</td><td class="muted">${rel.lastGoal()}</td></tr>`;
    }
  }
  relsTable.innerHTML = rh || '<tr><td colspan="4" class="muted">No relationships yet</td></tr>';

  // Bounties (hide completed)
  let bh = "";
  for (let i = 0; i < s.bountiesLength(); i++) {
    const b = s.bounties(i)!;
    const state = b.state();
    if (state === BountyStatus.Completed) continue;
    const desc = b.description() ?? "";
    bh += `<tr title="${desc.replace(/"/g, '&quot;')}"><td>${desc}</td><td>${b.rewardGold()}g</td><td class="${BOUNTY_CLASSES[state] ?? ""}">${BOUNTY_STATES[state] ?? "?"}</td><td>${b.claimedBy() || "-"}</td></tr>`;
  }
  bountiesTable.innerHTML = bh || '<tr><td colspan="4" class="muted">No active bounties</td></tr>';

  // Structures
  let sh = "";
  for (let i = 0; i < s.structuresLength(); i++) {
    const st = s.structures(i)!;
    const pos = st.pos();
    let items = "";
    for (let j = 0; j < st.inventoryLength(); j++) {
      const slot = st.inventory(j);
      if (slot) items += (items ? ", " : "") + `${slot.itemType()} x${slot.count()}`;
    }
    sh += `<tr><td>${st.spriteType()}</td><td>(${pos?.x()}, ${pos?.y()})</td><td>${items || "-"}</td></tr>`;
  }
  structuresTable.innerHTML = sh;

  // Activity log
  const logEl = document.getElementById("activity-log")!;
  const logPanel = document.getElementById("log-panel")!;
  let logHtml = "";
  for (let i = 0; i < s.eventLogLength(); i++) {
    const entry = s.eventLog(i)!;
    const agent = entry.agent() ?? "?";
    const color = agentColor(agent);
    const kind = entry.kind() ?? "";
    const text = entry.text() ?? "";
    const entryTick = entry.tick();

    let styled = "";
    switch (kind) {
      case "thought":
        styled = `<span class="log-thought">${text}</span>`;
        break;
      case "action":
      case "decision":
        styled = `<span class="log-decision">${text}</span>`;
        break;
      case "speech":
        styled = `<span class="log-speech">${text}</span>`;
        break;
      default:
        styled = `<span class="log-system">${text}</span>`;
    }
    logHtml += `<div class="log-entry"><span class="log-tick">${entryTick}</span><span style="color:${color};font-weight:bold;">${agent}</span> ${styled}</div>`;
  }
  if (logHtml) {
    logEl.innerHTML = logHtml;
    logPanel.scrollTop = logPanel.scrollHeight;
  }

  const q = s.boardQueue();
  if (q) {
    const w: string[] = [];
    for (let i = 0; i < q.waitingLength(); i++) w.push(q.waiting(i) ?? "?");
    queueInfo.innerHTML = `<strong>At board:</strong> ${q.interacting() || "none"} &nbsp;|&nbsp; <strong>Queue:</strong> ${w.length > 0 ? w.join(", ") : "empty"}`;
  }
}

const AGENT_COLORS: Record<string, string> = {
  "Alice": "#ff6b6b",
  "Bob": "#4ecdc4",
  "Carol": "#ffe66d",
  "SYSTEM": "#635bff",
};
function agentColor(name: string): string {
  return AGENT_COLORS[name] || "#888";
}

connect();

// Contract creation form
const form = document.getElementById("contract-form") as HTMLFormElement;
const status = document.getElementById("contract-status")!;

form?.addEventListener("submit", async (e) => {
  e.preventDefault();
  const data = new FormData(form);
  const title = data.get("title") as string;
  const description = data.get("description") as string;
  const reward = parseInt(data.get("reward") as string);
  const ttl = parseInt(data.get("ttl") as string);

  const steps: any[] = [];
  const descLower = description.toLowerCase();
  if (descLower.includes("google") || descLower.includes("search")) {
    steps.push({ description: "Spend 1g at Google", type: "spend_gold", building: "google", amount: 1 });
    steps.push({ description: "Perform web search", type: "web_search", min_count: 1 });
  }
  if (descLower.includes("report") || descLower.includes("document") || descLower.includes("write")) {
    steps.push({ description: "Produce document", type: "produce_document", title: `${title.replace(/\s+/g, "_").toLowerCase()}.md` });
  }
  if (descLower.includes("visit") || descLower.includes("go to")) {
    const buildings = ["cafe", "library", "warehouse", "shop", "restaurant", "office", "google", "hotel"];
    for (const b of buildings) {
      if (descLower.includes(b)) {
        steps.push({ description: `Visit ${b}`, type: "visit_building", building: b });
      }
    }
  }
  steps.push({ description: "Return to bounty board", type: "return_to_board" });

  try {
    const resp = await fetch("/api/contracts", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title, description, reward_gold: reward, ttl_ticks: ttl, steps }),
    });
    const result = await resp.json();
    status.textContent = `Created: ${result.title} (${result.step_count} steps)`;
    status.style.color = "#4caf50";
    setTimeout(() => { status.textContent = ""; }, 3000);
  } catch (err) {
    status.textContent = `Error: ${err}`;
    status.style.color = "#f44336";
  }
});
