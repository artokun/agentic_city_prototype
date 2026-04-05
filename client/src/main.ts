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
const agentsTable = $("agents-table");
const bountiesTable = $("bounties-table");
const structuresTable = $("structures-table");
const queueInfo = $("queue-info");
const chatLog = $("chat-log");
const relsTable = $("rels-table");

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

function inv(getter: (i: number) => any, len: number): string {
  if (len === 0) return "-";
  const p: string[] = [];
  for (let i = 0; i < len; i++) { const s = getter(i); if (s) p.push(`${s.itemType()} x${s.count()}`); }
  return p.join(", ") || "-";
}

function render(s: WorldSnapshot) {
  tickCount.textContent = s.tick().toString();

  // Agents
  let ah = "";
  for (let i = 0; i < s.agentsLength(); i++) {
    const a = s.agents(i)!;
    const pos = a.pos();
    const anim = a.animation();
    const needs = a.needs();
    const action = a.currentAction();
    const actionTicks = a.actionTicksLeft();

    const needsHtml = needs
      ? `<div class="needs-row">${needBar(needs.energy(), "E")}${needBar(needs.hunger(), "H")}${needBar(needs.boredom(), "B")}</div>`
      : "-";

    const actionHtml = action ? `<span class="action">${action} (${actionTicks}t)</span>` : "";

    const visCount = a.visibleEntitiesLength();
    const trackCount = a.trackedEntitiesLength();
    const knownLocs = a.knownLocationCount();
    const perceptionHtml = `<span class="muted">${knownLocs} locs | ${visCount} vis | ${trackCount} trk</span>`;

    const agentName = a.name() ?? "?";
    ah += `<tr>
      <td style="color:${agentColor(agentName)};font-weight:bold;">${agentName}</td>
      <td>(${pos?.x()}, ${pos?.y()})</td>
      <td class="${ANIM_CLASSES[anim] ?? ""}">${ANIM_NAMES[anim] ?? "?"}</td>
      <td class="gold">${a.gold()}</td>
      <td class="inv">${inv((j) => a.inventory(j), a.inventoryLength())}</td>
      <td>${needsHtml}</td>
      <td>${perceptionHtml}</td>
      <td>${actionHtml}</td>
      <td class="thought">${a.thought()}</td>
    </tr>`;
  }
  agentsTable.innerHTML = ah;

  // Active chats (deduplicate: only show from first agent alphabetically in each pair)
  const seenConvos = new Set<string>();
  let chatHtml = "";
  for (let i = 0; i < s.agentsLength(); i++) {
    const a = s.agents(i)!;
    if (a.activeChatLength() > 0) {
      // Build a key from all speakers to deduplicate
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
    // Short title: first sentence or first 50 chars
    const shortDesc = desc.includes(".") ? desc.split(".")[0] : desc.substring(0, 50);
    bh += `<tr title="${desc.replace(/"/g, '&quot;')}" style="cursor:pointer" onclick="alert('${desc.replace(/'/g, "\\'")}')"><td>${shortDesc}</td><td>${b.rewardGold()}g</td><td class="${BOUNTY_CLASSES[state] ?? ""}">${BOUNTY_STATES[state] ?? "?"}</td><td>${b.claimedBy() || "-"}</td></tr>`;
  }
  bountiesTable.innerHTML = bh || '<tr><td colspan="4" class="muted">No active bounties</td></tr>';

  // Structures
  let sh = "";
  for (let i = 0; i < s.structuresLength(); i++) {
    const st = s.structures(i)!;
    const pos = st.pos();
    const items = inv((j) => st.inventory(j), st.inventoryLength());
    sh += `<tr><td>${st.spriteType()}</td><td>(${pos?.x()}, ${pos?.y()})</td><td>${items}</td></tr>`;
  }
  structuresTable.innerHTML = sh;

  // Board queue
  // Activity log.
  const logEl = document.getElementById("activity-log")!;
  const logPanel = document.getElementById("log-panel")!;
  let logHtml = "";
  for (let i = 0; i < s.eventLogLength(); i++) {
    const entry = s.eventLog(i)!;
    const agent = entry.agent() ?? "?";
    const color = agentColor(agent);
    const kind = entry.kind() ?? "";
    const text = entry.text() ?? "";
    const tick = entry.tick();

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

    logHtml += `<div class="log-entry"><span class="log-tick">${tick}</span><span style="color:${color};font-weight:bold;">${agent}</span> ${styled}</div>`;
  }
  if (logHtml) {
    logEl.innerHTML = logHtml;
    // Auto-scroll to bottom.
    logPanel.scrollTop = logPanel.scrollHeight;
  }

  const q = s.boardQueue();
  if (q) {
    const w: string[] = [];
    for (let i = 0; i < q.waitingLength(); i++) w.push(q.waiting(i) ?? "?");
    queueInfo.innerHTML = `<strong>At board:</strong> ${q.interacting() || "none"} &nbsp;|&nbsp; <strong>Queue:</strong> ${w.length > 0 ? w.join(", ") : "empty"}`;
  }
}

// Agent color assignments (deterministic).
const AGENT_COLORS: Record<string, string> = {
  "Alice": "#ff6b6b",
  "Bob": "#4ecdc4",
  "Carol": "#ffe66d",
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

  // Auto-generate steps based on description keywords
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
