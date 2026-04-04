import * as flatbuffers from "flatbuffers";
import { WorldSnapshot } from "./generated/world/world-snapshot.js";
import { BountyStatus } from "./generated/world/bounty-status.js";

const ANIM_NAMES = ["Idle", "Walking", "Working"];
const ANIM_CLASSES = ["anim-idle", "anim-walking", "anim-working"];
const BOUNTY_STATES = ["Available", "Claimed", "Completed"];
const BOUNTY_CLASSES = ["bounty-available", "bounty-claimed", "bounty-completed"];

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

    ah += `<tr>
      <td>${a.name()}</td>
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

  // Active chats
  let chatHtml = "";
  for (let i = 0; i < s.agentsLength(); i++) {
    const a = s.agents(i)!;
    if (a.activeChatLength() > 0) {
      for (let j = 0; j < a.activeChatLength(); j++) {
        const msg = a.activeChat(j)!;
        chatHtml += `<div class="chat-msg"><strong>${msg.speaker()}</strong>: ${msg.text()}</div>`;
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

  // Bounties
  let bh = "";
  for (let i = 0; i < s.bountiesLength(); i++) {
    const b = s.bounties(i)!;
    const state = b.state();
    bh += `<tr><td>${b.description()}</td><td>${b.rewardGold()}g</td><td class="${BOUNTY_CLASSES[state] ?? ""}">${BOUNTY_STATES[state] ?? "?"}</td><td>${b.claimedBy() || "-"}</td></tr>`;
  }
  bountiesTable.innerHTML = bh;

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
  const q = s.boardQueue();
  if (q) {
    const w: string[] = [];
    for (let i = 0; i < q.waitingLength(); i++) w.push(q.waiting(i) ?? "?");
    queueInfo.innerHTML = `<strong>At board:</strong> ${q.interacting() || "none"} &nbsp;|&nbsp; <strong>Queue:</strong> ${w.length > 0 ? w.join(", ") : "empty"}`;
  }
}

connect();
