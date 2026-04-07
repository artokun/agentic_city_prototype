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
const docModal = $("doc-modal");
const docTitleEl = $("doc-title");
const docContentEl = $("doc-content");
const libraryPanel = $("library-panel");

// Per-agent history (persisted across renders).
const agentThoughts: Record<string, string[]> = {};
const agentActions: Record<string, { tick: number; text: string }[]> = {};
// Conversation history — persists even after conversations end.
const conversationHistory: { speaker: string; text: string }[] = [];
const seenMessages = new Set<string>(); // dedup across all ticks
const MAX_HISTORY = 10;
const MAX_CONVO_HISTORY = 50;

let messageCount = 0;
setInterval(() => { msgRate.textContent = `${messageCount} msg/s`; messageCount = 0; }, 1000);

async function openDocumentModal(agentName: string, docTitle: string) {
  const agent = agentName.toLowerCase();
  const encodedTitle = encodeURIComponent(docTitle);
  const resp = await fetch(`/api/documents/${agent}/${encodedTitle}`);
  if (!resp.ok) {
    throw new Error(`Failed to load ${docTitle} (${resp.status})`);
  }
  const text = await resp.text();
  docTitleEl.textContent = docTitle;
  docContentEl.textContent = text;
  docModal.style.display = "flex";
}

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

function escapeHtml(text: string): string {
  return text
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
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
    const hasNamedDocs = Array.from({ length: a.inventoryLength() }, (_, idx) => a.inventory(idx))
      .some((slot) => (slot?.itemType() ?? "").startsWith("doc:"));
    for (let j = 0; j < a.inventoryLength(); j++) {
      const slot = a.inventory(j);
      if (slot) {
        const itemName = slot.itemType() ?? "?";
        if (itemName === "document" && hasNamedDocs) continue;
        const cls = itemClass(itemName);
        if (itemName.startsWith("doc:")) {
          const docTitle = itemName.substring(4);
          invHtml += `<button class="item item-doc doc-link" type="button" data-agent="${escapeHtml(agentName)}" data-doc-title="${escapeHtml(docTitle)}" title="Click to read">${escapeHtml(docTitle)}</button>`;
        } else {
          invHtml += `<span class="item ${cls}">${escapeHtml(itemName)}${slot.count() > 1 ? ` x${slot.count()}` : ""}</span>`;
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

  // Conversations — accumulate into persistent history.
  const seenConvos = new Set<string>();
  for (let i = 0; i < s.agentsLength(); i++) {
    const a = s.agents(i)!;
    if (a.activeChatLength() > 0) {
      const speakers = new Set<string>();
      for (let j = 0; j < a.activeChatLength(); j++) speakers.add(a.activeChat(j)!.speaker() ?? "?");
      speakers.add(a.name() ?? "?");
      const key = [...speakers].sort().join("+");
      if (seenConvos.has(key)) continue;
      seenConvos.add(key);
      for (let j = 0; j < a.activeChatLength(); j++) {
        const msg = a.activeChat(j)!;
        const speaker = msg.speaker() ?? "?";
        const text = msg.text() ?? "";
        const key = `${speaker}:${text}`;
        if (!seenMessages.has(key)) {
          seenMessages.add(key);
          conversationHistory.push({ speaker, text });
          if (conversationHistory.length > MAX_CONVO_HISTORY) conversationHistory.shift();
        }
      }
    }
  }
  // Also capture SYSTEM speech from activity log.
  for (let i = 0; i < s.eventLogLength(); i++) {
    const entry = s.eventLog(i)!;
    if (entry.kind() === "speech") {
      const agent = entry.agent() ?? "?";
      const text = entry.text() ?? "";
      // Strip "[to X] " prefix for dedup — activeChat has raw text, eventLog has "[to Partner] text".
      const rawText = text.replace(/^\[to [^\]]+\]\s*/, "");
      const key = `${agent}:${rawText}`;
      if (!seenMessages.has(key)) {
        seenMessages.add(key);
        conversationHistory.push({ speaker: agent, text });
        if (conversationHistory.length > MAX_CONVO_HISTORY) conversationHistory.shift();
      }
    }
  }
  let chatHtml = "";
  for (const msg of conversationHistory) {
    chatHtml += `<div class="chat-msg"><strong style="color:${agentColor(msg.speaker)}">${msg.speaker}</strong>: ${msg.text}</div>`;
  }
  chatLog.innerHTML = chatHtml || '<span class="muted">No conversations yet</span>';
  const chatAutoScroll = document.getElementById("chat-autoscroll") as HTMLInputElement;
  if (chatHtml && chatAutoScroll?.checked) chatLog.scrollTop = chatLog.scrollHeight;

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
    const logAutoScroll = document.getElementById("log-autoscroll") as HTMLInputElement;
    if (logAutoScroll?.checked) logPanel.scrollTop = logPanel.scrollHeight;
  }

  const q = s.boardQueue();
  if (q) {
    const w: string[] = [];
    for (let i = 0; i < q.waitingLength(); i++) w.push(q.waiting(i) ?? "?");
    queueInfo.innerHTML = `<strong>At board:</strong> ${q.interacting() || "none"} &nbsp;|&nbsp; <strong>Queue:</strong> ${w.length > 0 ? w.join(", ") : "empty"}`;
  }

  updateMap(s);
}

const AGENT_COLORS: Record<string, string> = {
  "Alice Haiku": "#ff6b6b",
  "Bob Sonnet": "#4ecdc4",
  "Carol Opus": "#ffe66d",
  "Dave GPT": "#a8e6cf",
  "SYSTEM": "#635bff",
};
function agentColor(name: string): string {
  return AGENT_COLORS[name] || "#888";
}

connect();

// Library panel — poll every 5 seconds.
interface LibraryDoc { title: string; author: string; bounty: string; tick: number; content: string; }
let libraryCache: LibraryDoc[] = [];

async function fetchLibrary() {
  try {
    const resp = await fetch("/api/library");
    if (resp.ok) {
      libraryCache = await resp.json();
      renderLibrary();
    }
  } catch { /* ignore */ }
}

function renderLibrary() {
  if (libraryCache.length === 0) {
    libraryPanel.innerHTML = '<span class="muted">No documents archived yet</span>';
    return;
  }
  let html = "";
  for (const doc of libraryCache) {
    html += `<div class="lib-entry" data-lib-title="${escapeHtml(doc.title)}" data-lib-author="${escapeHtml(doc.author)}">
      <div class="lib-title">${escapeHtml(doc.title)}</div>
      <div class="lib-meta">by ${escapeHtml(doc.author)} — ${escapeHtml(doc.bounty)}</div>
    </div>`;
  }
  libraryPanel.innerHTML = html;
}

libraryPanel.addEventListener("click", (event) => {
  const entry = (event.target as HTMLElement).closest(".lib-entry") as HTMLElement | null;
  if (!entry) return;
  const title = entry.dataset.libTitle ?? "";
  const author = entry.dataset.libAuthor ?? "";
  const doc = libraryCache.find(d => d.title === title && d.author === author);
  if (doc) {
    docTitleEl.textContent = `${doc.title} (by ${doc.author})`;
    docContentEl.textContent = doc.content;
    docModal.style.display = "flex";
  }
});

fetchLibrary();
setInterval(fetchLibrary, 5000);

agentCardsEl.addEventListener("click", async (event) => {
  const target = event.target as HTMLElement | null;
  const docLink = target?.closest(".doc-link") as HTMLElement | null;
  if (!docLink) return;

  const agentName = docLink.dataset.agent;
  const title = docLink.dataset.docTitle;
  if (!agentName || !title) return;

  try {
    await openDocumentModal(agentName, title);
  } catch (err) {
    docTitleEl.textContent = title;
    docContentEl.textContent = err instanceof Error ? err.message : String(err);
    docModal.style.display = "flex";
  }
});

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
nt: string; }
let libraryCache: LibraryDoc[] = [];

async function fetchLibrary() {
  try {
    const resp = await fetch("/api/library");
    if (resp.ok) {
      libraryCache = await resp.json();
      renderLibrary();
    }
  } catch { /* ignore */ }
}

function renderLibrary() {
  if (libraryCache.length === 0) {
    libraryPanel.innerHTML = '<span class="muted">No documents archived yet</span>';
    return;
  }
  let html = "";
  for (const doc of libraryCache) {
    html += `<div class="lib-entry" data-lib-title="${escapeHtml(doc.title)}" data-lib-author="${escapeHtml(doc.author)}">
      <div class="lib-title">${escapeHtml(doc.title)}</div>
      <div class="lib-meta">by ${escapeHtml(doc.author)} — ${escapeHtml(doc.bounty)}</div>
    </div>`;
  }
  libraryPanel.innerHTML = html;
}

libraryPanel.addEventListener("click", (event) => {
  const entry = (event.target as HTMLElement).closest(".lib-entry") as HTMLElement | null;
  if (!entry) return;
  const title = entry.dataset.libTitle ?? "";
  const author = entry.dataset.libAuthor ?? "";
  const doc = libraryCache.find(d => d.title === title && d.author === author);
  if (doc) {
    docTitleEl.textContent = `${doc.title} (by ${doc.author})`;
    docContentEl.textContent = doc.content;
    docModal.style.display = "flex";
  }
});

fetchLibrary();
setInterval(fetchLibrary, 5000);

agentCardsEl.addEventListener("click", async (event) => {
  const target = event.target as HTMLElement | null;
  const docLink = target?.closest(".doc-link") as HTMLElement | null;
  if (!docLink) return;

  const agentName = docLink.dataset.agent;
  const title = docLink.dataset.docTitle;
  if (!agentName || !title) return;

  try {
    await openDocumentModal(agentName, title);
  } catch (err) {
    docTitleEl.textContent = title;
    docContentEl.textContent = err instanceof Error ? err.message : String(err);
    docModal.style.display = "flex";
  }
});

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
