// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let labels = {}; // address (lowercase) -> label
let last = null; // last simulation result, re-rendered when labels change

// ---------------------------------------------------------------------------
// HTML helpers
// ---------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

// Summary rows: row() escapes the value, rowh() takes raw HTML.
const row = (k, v) => `<dt>${k}</dt><dd>${esc(v)}</dd>`;
const rowh = (k, h) => `<dt>${k}</dt><dd>${h}</dd>`;

// Table cells: td() escapes the value, tdh() takes raw HTML.
const td = (v, cls = "") => `<td class="${cls}">${esc(v)}</td>`;
const tdh = (h) => `<td>${h}</td>`;

const table = (title, heads, rows) =>
  `<h2>${title}</h2>` +
  (rows.length
    ? `<table><tr>${heads.map((h) => `<th>${h}</th>`).join("")}</tr>${rows.join("")}</table>`
    : `<div class="muted">None</div>`);

// Clickable address (HTML). bare=true shows just the address (used in the trace).
const name = (a, bare = false) => {
  const l = labels[String(a).toLowerCase()];
  return `<a href="#" class="addr" data-addr="${esc(a)}" title="Click to set label">${esc(l && !bare ? `${l} (${a})` : a)}</a>`;
};

// ---------------------------------------------------------------------------
// Simulation result rendering
// ---------------------------------------------------------------------------

function render(d) {
  $("status").textContent = d.success ? "✓ Success" : "✗ Reverted";
  $("status").className = "status " + (d.success ? "ok" : "bad");

  $("summary").innerHTML =
    rowh("Safe", name(d.safe)) +
    rowh("To", name(d.to)) +
    row("Value", d.value + " wei") +
    row("Operation", d.operation === 1 ? "1 (delegatecall)" : "0 (call)") +
    row("Nonce", d.nonce) +
    rowh("Owner (sim)", name(d.owner));

  $("balances").innerHTML = d.tokensError
    ? `<h2>Token summary</h2><div class="muted">Unavailable: ${esc(d.tokensError)}</div>`
    : table(
      "Balance changes",
      ["Holder", "Token", "Change"],
      d.balances.map(
        (b) =>
          `<tr>${tdh(name(b.holder))}${td(b.symbol)}${td(b.delta, b.delta.startsWith("-") ? "neg" : "pos")}</tr>`,
      ),
    );

  $("transfers").innerHTML = d.tokensError
    ? ""
    : table(
      `Transfers (${d.transfers.length})`,
      ["Token", "From", "To", "Amount"],
      d.transfers.map(
        (t) => `<tr>${td(t.symbol)}${tdh(name(t.from))}${tdh(name(t.to))}${td(t.amount)}</tr>`,
      ),
    );

  // \b on both ends: a 40-hex run inside a longer hex word (hashes, slots) is not an address
  $("trace").innerHTML = esc(d.trace || "(no trace output)").replace(
    /\b0x[0-9a-fA-F]{40}\b/g,
    (m) => name(m, true),
  );

  $("card").classList.add("show");
}

// ---------------------------------------------------------------------------
// Address book
// ---------------------------------------------------------------------------

function renderBook() {
  const entries = Object.entries(labels).sort((a, b) => a[1].localeCompare(b[1]));
  $("book-n").textContent = entries.length;
  $("book-rows").innerHTML = entries
    .map(
      ([a, l]) =>
        `<tr><td>${esc(a)}</td><td><input data-a="${esc(a)}" value="${esc(l)}" maxlength="64"></td>` +
        `<td><button class="x" data-del="${esc(a)}" title="Delete">✕</button></td></tr>`,
    )
    .join("");
}

async function saveBook(book) {
  $("book-err").style.display = "none";
  try {
    const res = await fetch("/addressbook", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ book }),
    });
    const d = await res.json();
    if (!d.ok) throw new Error(d.error);
    labels = d.book;
    if (last) render(last); // trace keeps old labels until the next simulation
  } catch (err) {
    $("book-err").textContent = err.message;
    $("book-err").style.display = "block";
  }
  renderBook();
}

// Add / update form.
$("book-add").addEventListener("submit", async (e) => {
  e.preventDefault();
  await saveBook({
    ...labels,
    [$("book-addr").value.trim().toLowerCase()]: $("book-label").value.trim(),
  });
  if ($("book-err").style.display === "none") $("book-add").reset();
});

// Click any address on the page to set / change / clear its label.
document.addEventListener("click", (e) => {
  const el = e.target.closest(".addr");
  if (!el) return;
  e.preventDefault();
  const a = el.dataset.addr.toLowerCase();
  const l = prompt(`Label for ${el.dataset.addr} (empty to remove)`, labels[a] || "");
  if (l === null) return;
  const { [a]: _, ...rest } = labels;
  saveBook(l.trim() ? { ...rest, [a]: l.trim() } : rest);
});

// Delete button in the book table.
$("book-rows").addEventListener("click", (e) => {
  const a = e.target.dataset.del;
  if (a) {
    const { [a]: _, ...rest } = labels;
    saveBook(rest);
  }
});

// Inline label edit in the book table.
$("book-rows").addEventListener("change", (e) => {
  const a = e.target.dataset.a;
  if (!a) return;
  const { [a]: _, ...rest } = labels,
    l = e.target.value.trim();
  saveBook(l ? { ...rest, [a]: l } : rest); // cleared label = delete
});

// ---------------------------------------------------------------------------
// My Safes + pending transactions
// ---------------------------------------------------------------------------

let safes = []; // saved Safe addresses (lowercase)

function renderSafes() {
  $("safes-n").textContent = safes.length;
  $("safes-rows").innerHTML = safes
    .map(
      (a) =>
        `<tr><td>${name(a)}</td>` +
        `<td><button class="x" data-del="${esc(a)}" title="Remove">✕</button></td></tr>`,
    )
    .join("");
}

const showSafesErr = (msg) => {
  $("safes-err").textContent = msg;
  $("safes-err").style.display = msg ? "block" : "none";
};

async function saveSafes(list) {
  showSafesErr("");
  try {
    const res = await fetch("/safes", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ safes: list }),
    });
    const d = await res.json();
    if (!d.ok) throw new Error(d.error);
    safes = d.safes;
    loadPending();
  } catch (err) {
    showSafesErr(err.message);
  }
  renderSafes();
}

async function loadPending() {
  if (!safes.length) {
    $("pending").innerHTML = "";
    return;
  }
  $("pending-go").disabled = true;
  $("pending").innerHTML = '<div class="muted">Loading pending transactions…</div>';
  try {
    const d = await (await fetch("/pending")).json();
    if (!d.ok) throw new Error(d.error);
    const errs = d.errors.map((e) => `<div class="neg">${name(e.safe)}: ${esc(e.error)}</div>`).join("");
    $("pending").innerHTML =
      errs +
      table(
        `Pending (${d.pending.length})`,
        ["Safe", "Nonce", "Call", "Confirmations", "Submitted", ""],
        d.pending.map(
          (t) =>
            `<tr>${tdh(name(t.safe))}${td(t.nonce)}` +
            tdh(`${esc(t.method || "call")} → ${name(t.to)}`) +
            td(`${t.confirmations}/${t.confirmationsRequired}`) +
            td((t.submissionDate || "").slice(0, 16).replace("T", " ")) +
            tdh(`<button data-sim="${esc(t.safeTxHash)}">Simulate</button>`) +
            `</tr>`,
        ),
      );
  } catch (err) {
    $("pending").innerHTML = `<div class="neg">${esc(err.message)}</div>`;
  } finally {
    $("pending-go").disabled = false;
  }
}

$("safes-add").addEventListener("submit", async (e) => {
  e.preventDefault();
  await saveSafes([...safes, $("safes-addr").value.trim()]);
  if ($("safes-err").style.display === "none") $("safes-add").reset();
});

$("safes-rows").addEventListener("click", (e) => {
  const a = e.target.dataset.del;
  if (a) saveSafes(safes.filter((s) => s !== a));
});

$("pending-go").addEventListener("click", loadPending);

// Simulate a pending tx: reuse the hash form as-is.
$("pending").addEventListener("click", (e) => {
  const h = e.target.dataset.sim;
  if (!h) return;
  $("hash").value = h;
  $("f").requestSubmit();
  $("card").scrollIntoView({ behavior: "smooth" });
});

// ---------------------------------------------------------------------------
// Simulation form
// ---------------------------------------------------------------------------

async function simulate(e) {
  e.preventDefault();
  const safeTxHash = $("hash").value.trim();
  if (!safeTxHash) return;

  $("err").style.display = "none";
  $("card").classList.remove("show");
  $("go").disabled = true;
  $("go").innerHTML = '<span class="spin"></span>Simulating…';

  try {
    const res = await fetch("/simulate", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ safeTxHash }),
    });
    const d = await res.json();
    if (!d.ok) throw new Error(d.error || "Simulation failed");

    labels = d.labels || {};
    last = d;
    render(d);
    renderBook();
  } catch (err) {
    $("err").textContent = err.message;
    $("err").style.display = "block";
  } finally {
    $("go").disabled = false;
    $("go").textContent = "Simulate";
  }
}

$("f").addEventListener("submit", simulate);

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

fetch("/addressbook")
  .then((r) => r.json())
  .then((d) => {
    labels = d.book || {};
    renderBook();
    renderSafes(); // labels may arrive after the Safes list
  });

fetch("/safes")
  .then((r) => r.json())
  .then((d) => {
    safes = d.safes || [];
    renderSafes();
    loadPending();
  });
