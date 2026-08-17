/* Lambo portal.
 *
 * A reader. It fetches, it renders, it never mutates. Plain DOM on purpose:
 * these three files are compiled into the binary with include_str!, so there is
 * no build step, no framework and no external asset. `lambo serve-web` works on
 * a machine with nothing installed and no network, and that is worth more than
 * any convenience a bundler would buy.
 *
 * Panels whose endpoint does not answer hide themselves rather than rendering a
 * placeholder. A page that draws a structure it cannot actually see is worse
 * than a page with no structure panel.
 */
(function () {
  "use strict";

  // ---- vocabulary ------------------------------------------------------
  // Entity, Constraint, Logic, Resource, Observation, Candidate, Venerable and
  // Canonical are the product's own terms and are shown verbatim. They are
  // taught with a tooltip and a legend rather than translated into softer
  // words, because a gloss that replaces the term teaches nobody the term.

  var KIND_TOOLTIP = {
    Entity: "A thing the work is about, like a file, a table, or a service.",
    Constraint: 'A rule the work must keep following, like "passwords must be hashed."',
    Resource: "Something the agents built or changed, like a file they wrote.",
    Logic: "A decision the agents made and why, like why they picked this approach.",
    Observation: "Something an agent happened to notice. It is dropped first when memories are cleaned up."
  };

  var STATUS_TOOLTIP = {
    Candidate: "Recorded, and holding up so far. Most memories stay here.",
    Venerable: "Survived repeated cleanup passes while other memories were dropped.",
    Canonical: "Enough other work depends on this that changing it is dangerous. Lambo warns any agent that touches it."
  };

  var LADDER = [
    { key: "Canonical", desc: "Enough other work depends on this that changing it is dangerous. Lambo warns any agent that touches it." },
    { key: "Venerable", desc: "Survived repeated cleanup passes while other memories were dropped." },
    { key: "Candidate", desc: "Recorded, and holding up so far. Most memories stay here." },
    { key: "none", desc: "Most memories stay here, and that is normal." }
  ];

  // Phrased from the dependent's side, because that is whose name the label is
  // printed next to. "contains" beside `role column` reads as "role column
  // contains", which is the relationship backwards.
  var EDGE_LABEL = { Hierarchical: "part of it", Dependency: "depends on it", Causal: "caused by it" };

  var GATE_LABEL = {
    gc_survived: "Survived cleanup passes",
    blast_radius: "How much depends on it",
    distinct_interactions: "Separate interactions",
    coverage: "Spread across the session"
  };

  var COUNT_LABEL = [
    ["nodes", "Things remembered"],
    ["concepts", "Distinct memories"],
    ["edges", "Connections"],
    ["canonical", "Canonical"],
    ["canonization_events", "Status changes"]
  ];

  // Live in the writer process. A reader cannot observe them, so they are shown
  // as unavailable rather than as a zero that reads like a real count.
  var WRITER_ONLY = [
    ["Flush lag", "Measured in the writing process"],
    ["Log depth", "Measured in the writing process"],
    ["Daemon cycles", "Measured in the writing process"]
  ];

  // ---- tiny DOM helpers ------------------------------------------------

  function $(id) { return document.getElementById(id); }
  function show(el, on) { if (el) el.classList.toggle("hidden", !on); }
  function clear(el) { while (el && el.firstChild) el.removeChild(el.firstChild); }

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = String(text);
    return n;
  }

  function kindBadge(kind) {
    var n = el("span", "kind k-" + kind, kind);
    n.title = KIND_TOOLTIP[kind] || kind;
    return n;
  }

  function statusBadge(status) {
    if (!status || status === "None") return null;
    var n = el("span", "status s-" + status, status);
    n.title = STATUS_TOOLTIP[status] || status;
    return n;
  }

  function plural(n, one, many) { return n === 1 ? one : many; }

  function get(path) {
    return fetch(path, { headers: { accept: "application/json" } }).then(function (r) {
      if (!r.ok) { var e = new Error("HTTP " + r.status); e.status = r.status; throw e; }
      return r.json();
    });
  }

  // ---- state -----------------------------------------------------------

  var state = {
    pollMs: 1500,
    graph: null,          // null until /api/graph answers; stays null on 404
    focus: null,
    expanded: {},
    seen: 0,
    events: [],
    failures: 0,
    painted: false,
    lookupSeq: 0,
    showFallback: false,
    lastResult: null
  };

  // ---- theme -----------------------------------------------------------
  // Three states: system (default, no attribute), light, dark.

  var THEMES = ["system", "light", "dark"];

  function applyTheme(t) {
    if (t === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", t);
    $("theme-btn").textContent = "Theme: " + t;
    try { localStorage.setItem("lambo-theme", t); } catch (e) { /* private mode */ }
  }

  function initTheme() {
    var saved = null;
    try { saved = localStorage.getItem("lambo-theme"); } catch (e) { /* ignore */ }
    var t = THEMES.indexOf(saved) >= 0 ? saved : "system";
    applyTheme(t);
    $("theme-btn").addEventListener("click", function () {
      var next = THEMES[(THEMES.indexOf(t) + 1) % THEMES.length];
      t = next;
      applyTheme(next);
    });
  }

  // ---- session facts ---------------------------------------------------

  function renderSession(info) {
    $("session-name").textContent = info.session;
    state.pollMs = info.poll_interval_ms || 1500;

    var facts = [
      ["Session", info.session],
      ["Memory stored in", info.store],
      ["Meaning model", info.embedder + (info.embedding_dim ? ", " + info.embedding_dim + " dimensions" : "")],
      ["Search by meaning", info.vector_search ? "On" : "Off"],
      ["This view", info.mode === "reader" ? "Reads a copy" : info.mode],
      ["Refreshes", (state.pollMs / 1000).toFixed(1) + "s"]
    ];
    var wrap = $("facts");
    clear(wrap);
    facts.forEach(function (f) {
      var d = el("div");
      d.appendChild(el("div", "fact-label", f[0]));
      d.appendChild(el("div", "fact-value", f[1]));
      wrap.appendChild(d);
    });

    $("refresh-note").textContent = "Every " + (state.pollMs / 1000).toFixed(1) + "s";
    $("footer").textContent =
      "This view reads a copy of the memory and cannot write to it. Nothing here can pin, " +
      "promote, edit or delete. Refreshes every " + (state.pollMs / 1000).toFixed(1) +
      "s. Lambo " + (info.version || "") + ".";
  }

  // ---- counts ----------------------------------------------------------

  function renderCounts(stats) {
    var wrap = $("counts");
    clear(wrap);
    COUNT_LABEL.forEach(function (c) {
      var card = el("div", "count-card");
      card.appendChild(el("div", "count-num", stats[c[0]] === undefined ? "0" : stats[c[0]]));
      card.appendChild(el("div", "count-label", c[1]));
      wrap.appendChild(card);
    });
    WRITER_ONLY.forEach(function (u) {
      var card = el("div", "count-card is-unavailable");
      card.title = stats.writer_only || u[1];
      card.appendChild(el("div", "count-num", "—"));
      card.appendChild(el("div", "count-label", u[0]));
      card.appendChild(el("div", "count-note", "Not visible to a reader"));
      wrap.appendChild(card);
    });
  }

  // ---- ladder ----------------------------------------------------------
  // The population at each rung is replayed from the transition feed, because
  // the API publishes transitions rather than a roll-call. When the structure
  // payload is available it is the better source, since it names every concept
  // including the large majority that never earned a rung.

  function ladderCounts() {
    var counts = { Canonical: 0, Venerable: 0, Candidate: 0, none: 0 };
    if (state.graph) {
      state.graph.nodes.forEach(function (n) {
        var k = (!n.status || n.status === "None") ? "none" : n.status;
        if (counts[k] !== undefined) counts[k]++;
      });
      return counts;
    }
    var latest = {};
    state.events.forEach(function (e) { latest[e.content] = e.to_status; });
    Object.keys(latest).forEach(function (name) {
      var k = latest[name];
      if (counts[k] !== undefined) counts[k]++;
    });
    return counts;
  }

  function renderLadder() {
    var counts = ladderCounts();
    var max = Math.max(1, counts.Canonical, counts.Venerable, counts.Candidate, counts.none);
    var wrap = $("ladder");
    clear(wrap);

    LADDER.forEach(function (d) {
      var count = counts[d.key];
      var weighted = d.key === "Canonical" || d.key === "Venerable";
      var row = el("div", "ladder-row r-" + d.key + (weighted ? " is-weighted" : ""));

      var bar = el("div", "ladder-bar");
      bar.style.width = Math.max(6, Math.round((count / max) * 100)) + "%";
      bar.appendChild(el("span", "ladder-label", d.key === "none" ? "No status" : d.key));
      bar.appendChild(el("span", "ladder-count", count));
      row.appendChild(bar);
      row.appendChild(el("div", "ladder-desc", d.desc));
      wrap.appendChild(row);
    });
  }

  // ---- history ---------------------------------------------------------

  function renderHistory() {
    var wrap = $("audit");
    clear(wrap);
    var rows = state.events.slice(-40);
    show($("audit-empty"), rows.length === 0);
    show(wrap, rows.length > 0);

    rows.forEach(function (e) {
      var row = el("div", "audit-row");
      var t = e.occurred_at ? new Date(e.occurred_at) : null;
      row.appendChild(el("span", "audit-time", t ? t.toLocaleTimeString() : ""));

      // Written as a sentence. "user schema became Canonical" is what the row
      // means; a four column grid with no header makes the reader work that
      // out from position alone.
      var sentence = el("span", "audit-sentence");
      sentence.appendChild(el("span", "audit-name", e.content));
      var promoted = !e.from_status || e.from_status === "None";
      sentence.appendChild(document.createTextNode(promoted ? " became " : " went from " + e.from_status + " to "));
      sentence.appendChild(el("span", "audit-to", e.to_status));
      row.appendChild(sentence);

      // Only frozen at the moment something becomes Canonical. Elsewhere it is
      // legitimately absent, so nothing is rendered rather than a zero.
      if (e.blast_radius !== null && e.blast_radius !== undefined) {
        row.appendChild(el("span", "audit-blast",
          e.blast_radius + " " + plural(e.blast_radius, "thing", "things") + " depended on it"));
      }
      wrap.appendChild(row);
    });
  }

  // ---- legend ----------------------------------------------------------

  function renderLegend() {
    var wrap = $("legend");
    clear(wrap);
    ["Entity", "Constraint", "Resource", "Logic", "Observation"].forEach(function (k) {
      var item = el("div", "legend-item");
      item.appendChild(kindBadge(k));
      item.appendChild(el("span", "legend-text", KIND_TOOLTIP[k]));
      wrap.appendChild(item);
    });
  }

  // ---- hero ------------------------------------------------------------
  // Whatever this session's most depended-on memory is, not a hardcoded name.
  // On a session where nothing has earned anything, it says so plainly.

  function pickPillar() {
    if (!state.graph || !state.graph.nodes.length) return null;
    var canon = state.graph.nodes.filter(function (n) { return n.status === "Canonical"; });
    var pool = canon.length ? canon : state.graph.nodes.filter(function (n) { return n.blast_radius > 0; });
    if (!pool.length) return null;
    return pool.slice().sort(function (a, b) { return b.blast_radius - a.blast_radius; })[0];
  }

  function renderHero() {
    var pillar = pickPillar();
    show($("hero-filled"), !!pillar);
    show($("hero-empty"), !pillar);

    if (!pillar) {
      $("hero-empty-msg").textContent = state.graph
        ? "Nothing here has enough depending on it yet. Memories are being recorded; status has to be earned."
        : "Waiting for the session's structure.";
      return;
    }

    $("hero-heading").textContent = pillar.content;
    var k = $("hero-kind");
    k.className = "kind k-" + pillar.concept_type;
    k.textContent = pillar.concept_type;
    k.title = KIND_TOOLTIP[pillar.concept_type] || "";

    var s = $("hero-status");
    if (!pillar.status || pillar.status === "None") {
      show(s, false);
    } else {
      show(s, true);
      s.className = "status s-" + pillar.status;
      s.textContent = pillar.status;
      s.title = STATUS_TOOLTIP[pillar.status] || "";
    }

    $("hero-blast").textContent = pillar.blast_radius;
    $("hero-blast-caption").textContent = pillar.blast_radius === 1
      ? "other thing depends on this"
      : "other things depend on this";
    $("hero-sentence").textContent =
      pillar.status === "Canonical"
        ? "Lambo marks this Canonical, so any agent that moves to change it is warned first."
        : "More depends on this than on anything else in the session.";

    // Dependents come from the focus endpoint; until it answers we show what
    // the structure payload already knows.
    get("/api/inspect?focus=" + encodeURIComponent(pillar.content))
      .then(function (d) { fillHeroDeps(pillar, d.dependents || [], d.dependents ? d.dependents.length : 0); })
      .catch(function () { fillHeroDeps(pillar, [], pillar.blast_radius); });
  }

  function fillHeroDeps(pillar, deps, total) {
    var wrap = $("hero-deps");
    clear(wrap);
    deps.slice(0, 6).forEach(function (d) {
      var chip = el("div", "chip");
      chip.appendChild(el("span", "shape k-" + d.concept_type));
      chip.appendChild(el("span", "chip-name", d.content));
      chip.appendChild(el("span", "chip-edge", EDGE_LABEL[d.edge] || d.edge));
      wrap.appendChild(chip);
    });
    if (deps.length > 6) {
      wrap.appendChild(el("div", "chip", "+" + (deps.length - 6) + " more"));
    }
    $("hero-deps-label").textContent = "What depends on it (" + (total || deps.length) + ")";

    var btn = $("hero-inspect");
    show(btn, deps.length > 0);
    btn.textContent = "Inspect all " + (total || deps.length) + " " + plural(total || deps.length, "dependent", "dependents") + " →";
    btn.onclick = function () { setFocus(pillar.content); };
  }

  // ---- structure tree --------------------------------------------------

  function buildTree() {
    var wrap = $("tree");
    clear(wrap);
    if (!state.graph) return;

    var hier = state.graph.edges.filter(function (e) { return e.edge === "Hierarchical"; });
    var children = {};
    hier.forEach(function (e) { (children[e.parent] = children[e.parent] || []).push(e.child); });
    var childSet = {};
    hier.forEach(function (e) { childSet[e.child] = true; });

    var byName = {};
    state.graph.nodes.forEach(function (n) { byName[n.content] = n; });

    var roots = Object.keys(children).filter(function (p) { return !childSet[p]; }).sort();
    if (!roots.length) {
      // No containment recorded. Every concept is a root, which is the honest
      // rendering rather than an invented hierarchy.
      roots = state.graph.nodes.map(function (n) { return n.content; });
    }

    var seen = {};
    function walk(name, depth, path) {
      if (seen[path]) return;
      seen[path] = true;
      var node = byName[name] || { content: name, concept_type: "Entity", status: "None", blast_radius: 0 };
      var kids = (children[name] || []).slice().sort();
      var open = state.expanded[path] !== false;

      var row = el("div", "tree-row" + (state.focus === name ? " is-selected" : ""));
      row.style.paddingLeft = (8 + depth * 18) + "px";

      var toggle = el("button", "tree-toggle" + (kids.length ? "" : " is-leaf"), kids.length ? (open ? "▾" : "▸") : "·");
      if (kids.length) {
        toggle.setAttribute("aria-expanded", String(open));
        toggle.setAttribute("aria-label", (open ? "Collapse " : "Expand ") + name);
        toggle.onclick = function () { state.expanded[path] = !open; buildTree(); };
      } else {
        toggle.setAttribute("aria-hidden", "true");
        toggle.tabIndex = -1;
      }
      row.appendChild(toggle);

      var nameBtn = el("button", "tree-name", name);
      nameBtn.onclick = function () { setFocus(name); };
      row.appendChild(nameBtn);
      row.appendChild(kindBadge(node.concept_type));

      var sb = statusBadge(node.status);
      if (sb) row.appendChild(sb);
      if (node.blast_radius > 0) {
        row.appendChild(el("span", "tree-blast", node.blast_radius + " depend on it"));
      }
      wrap.appendChild(row);

      if (open) kids.forEach(function (c) { walk(c, depth + 1, path + "/" + c); });
    }

    roots.forEach(function (r) { walk(r, 0, r); });
    show($("tree-truncated"), !!state.graph.truncated);
  }

  // ---- details ---------------------------------------------------------

  function setFocus(name) {
    state.focus = name;
    try {
      var p = new URLSearchParams(window.location.search);
      p.set("focus", name);
      history.replaceState(null, "", "?" + p.toString());
    } catch (e) { /* ignore */ }
    buildTree();
    loadFocus(name);
  }

  function clearFocus() {
    state.focus = null;
    try {
      var p = new URLSearchParams(window.location.search);
      p.delete("focus");
      var q = p.toString();
      history.replaceState(null, "", q ? "?" + q : window.location.pathname);
    } catch (e) { /* ignore */ }
    show($("details-filled"), false);
    show($("details-empty"), true);
    buildTree();
  }

  function loadFocus(name) {
    get("/api/inspect?focus=" + encodeURIComponent(name))
      .then(renderDetails)
      .catch(function () {
        show($("details-filled"), false);
        show($("details-empty"), true);
      });
  }

  function renderDetails(d) {
    show($("details-empty"), false);
    show($("details-filled"), true);

    $("details-name").textContent = d.focus;

    var k = $("details-kind");
    var firstKind = (d.dependents && d.dependents.length) ? null : null;
    var node = state.graph && state.graph.nodes.filter(function (n) { return n.content === d.focus; })[0];
    var kind = node ? node.concept_type : firstKind;
    if (kind) {
      show(k, true);
      k.className = "kind k-" + kind;
      k.textContent = kind;
      k.title = KIND_TOOLTIP[kind] || "";
    } else { show(k, false); }

    var s = $("details-status");
    if (!d.status || d.status === "None") {
      show(s, false);
    } else {
      show(s, true);
      s.className = "status s-" + d.status;
      s.textContent = d.status;
      s.title = STATUS_TOOLTIP[d.status] || "";
    }

    $("details-blast").textContent = d.blast_radius;
    $("details-blast-caption").textContent = d.blast_radius === 1
      ? "thing depends on this right now"
      : "things depend on this right now";

    var deps = d.dependents || [];
    $("details-deps-label").textContent = "What depends on it (" + deps.length + ")";
    var wrap = $("details-deps");
    clear(wrap);
    deps.forEach(function (x) {
      var chip = el("div", "chip");
      chip.appendChild(el("span", "chip-name", x.content));
      chip.appendChild(kindBadge(x.concept_type));
      chip.appendChild(el("span", "chip-edge", EDGE_LABEL[x.edge] || x.edge));
      wrap.appendChild(chip);
    });
    show(wrap, deps.length > 0);
    show($("details-deps-empty"), deps.length === 0 && d.found);
    show($("details-truncated"), !!d.truncated);

    renderGates(d);
  }

  // The gates answer "why is this not Canonical yet", which is not a question
  // about something that already is. They also measure against aged
  // connections while the blast radius above counts live ones, so on a young
  // session the two disagree and the pairing reads as a bug. Suppressed here
  // until the server stops sending it (H2 in the hardening notes).
  function renderGates(d) {
    var gp = d.gate_progress;
    var applicable = gp && d.status !== "Canonical";
    show($("details-gates-wrap"), !!applicable);
    if (!applicable) return;

    var wrap = $("details-gates");
    clear(wrap);

    ["gc_survived", "blast_radius", "distinct_interactions", "coverage"].forEach(function (key) {
      var g = gp[key];
      if (!g) return;
      var isPct = key === "coverage";
      var row = el("div", "gate-row");

      var head = el("div", "gate-label-row");
      head.appendChild(el("span", null, GATE_LABEL[key] || key));
      var met = el("span", "gate-met" + (g.met ? " is-met" : ""), g.met ? "met" : "not met");
      head.appendChild(met);
      row.appendChild(head);

      var track = el("div", "gate-track");
      var fill = el("div", "gate-fill" + (g.met ? " is-met" : ""));
      fill.style.width = Math.max(0, Math.min(100, (g.current / g.bar) * 100)) + "%";
      track.appendChild(fill);
      row.appendChild(track);

      var fmt = function (v) { return isPct ? Math.round(v * 100) + "%" : (Math.round(v * 100) / 100); };
      row.appendChild(el("div", "muted-small",
        fmt(g.current) + " of " + (g.strictly_above ? "more than " : "") + fmt(g.bar)));
      wrap.appendChild(row);
    });

    var cd = $("details-cooldown");
    show(cd, !!gp.in_cooldown);
    if (gp.in_cooldown) {
      cd.textContent = "Recently downgraded, so it is in a cooling-off period before it can " +
        "become Canonical again. This is separate from the checks above: every one of them " +
        "can be met and promotion still waits.";
    }
  }

  // ---- lookup ----------------------------------------------------------

  var combo = { open: false, options: [], index: -1 };

  function comboOptions(q) {
    if (!state.graph) return [];
    var needle = (q || "").trim().toLowerCase();
    var pool = needle
      ? state.graph.nodes.filter(function (n) { return n.content.toLowerCase().indexOf(needle) >= 0; })
      : state.graph.nodes.slice();
    // Most depended-on first, so what matters surfaces without being explained.
    return pool.sort(function (a, b) { return b.blast_radius - a.blast_radius; }).slice(0, 8);
  }

  function renderCombo() {
    var box = $("lookup-listbox");
    var input = $("lookup-input");
    clear(box);
    show(box, combo.open);
    input.setAttribute("aria-expanded", String(combo.open));

    if (!combo.open) { input.removeAttribute("aria-activedescendant"); return; }

    if (!combo.options.length) {
      box.appendChild(el("div", "no-match", "No exact match. Press Enter to look it up anyway."));
      input.removeAttribute("aria-activedescendant");
      return;
    }

    combo.options.forEach(function (o, i) {
      var row = el("div", "option");
      row.id = "lookup-opt-" + i;
      row.setAttribute("role", "option");
      row.setAttribute("aria-selected", String(i === combo.index));
      row.appendChild(el("span", "option-name", o.content));
      var sb = statusBadge(o.status);
      if (sb) row.appendChild(sb);
      row.appendChild(el("span", "option-deps",
        o.blast_radius > 0 ? o.blast_radius + " depend on it" : "nothing depends on it"));
      row.addEventListener("mousedown", function (ev) {
        ev.preventDefault();
        input.value = o.content;
        closeCombo();
        runLookup(o.content);
      });
      box.appendChild(row);
    });

    if (combo.index >= 0) input.setAttribute("aria-activedescendant", "lookup-opt-" + combo.index);
    else input.removeAttribute("aria-activedescendant");
  }

  function openCombo() {
    combo.options = comboOptions($("lookup-input").value);
    combo.open = true;
    combo.index = -1;
    renderCombo();
  }

  function closeCombo() {
    combo.open = false;
    combo.index = -1;
    renderCombo();
  }

  function initLookup() {
    var input = $("lookup-input");

    input.addEventListener("input", openCombo);
    input.addEventListener("focus", openCombo);
    input.addEventListener("blur", function () { setTimeout(closeCombo, 120); });

    input.addEventListener("keydown", function (ev) {
      if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
        ev.preventDefault();
        if (!combo.open) { openCombo(); return; }
        var d = ev.key === "ArrowDown" ? 1 : -1;
        combo.index = (combo.index + d + combo.options.length + 1) % (combo.options.length + 1) - 1;
        renderCombo();
      } else if (ev.key === "Enter") {
        ev.preventDefault();
        // Free text is always accepted: the lookup handles phrasings that are
        // not names at all, and rejecting them would be wrong.
        var picked = combo.index >= 0 && combo.options[combo.index];
        if (picked) input.value = picked.content;
        closeCombo();
        runLookup(input.value);
      } else if (ev.key === "Escape") {
        closeCombo();
      }
    });

    $("lookup-btn").addEventListener("click", function () { runLookup(input.value); });
    $("fallback-toggle").addEventListener("click", function () {
      state.showFallback = !state.showFallback;
      paintResult();
    });
  }

  var STAGES = ["Embedding the query…", "Searching by meaning…", "Ranking results…"];

  function runLookup(query) {
    var q = (query || "").trim();
    if (!q) return;

    var seq = ++state.lookupSeq;
    var started = Date.now();
    var stage = 0;

    show($("lookup-results"), false);
    show($("lookup-loading"), true);
    $("lookup-stage").textContent = STAGES[0];
    $("lookup-btn").disabled = true;

    // Roughly four seconds in production, so the wait is designed rather than
    // left to a bare spinner.
    var timer = setInterval(function () {
      stage = Math.min(stage + 1, STAGES.length - 1);
      $("lookup-stage").textContent = STAGES[stage];
    }, 1300);

    get("/api/recall?q=" + encodeURIComponent(q))
      .then(function (r) {
        if (seq !== state.lookupSeq) return;
        state.lastResult = r;
        state.lastResult.clientMs = Date.now() - started;
        paintResult();
      })
      .catch(function (e) {
        if (seq !== state.lookupSeq) return;
        state.lastResult = { error: e.message, query: q };
        paintResult();
      })
      .then(function () {
        if (seq !== state.lookupSeq) return;
        clearInterval(timer);
        show($("lookup-loading"), false);
        $("lookup-btn").disabled = false;
      });
  }

  function paintResult() {
    var r = state.lastResult;
    if (!r) return;
    show($("lookup-results"), true);

    if (r.error) {
      $("lookup-timing").textContent = "";
      show($("lookup-cards"), false);
      show($("fallback-toggle"), false);
      show($("traversal-banner"), false);
      var pre = $("lookup-fallback");
      show(pre, true);
      pre.textContent = "The lookup did not complete: " + r.error;
      return;
    }

    var ms = r.elapsed_ms !== undefined ? r.elapsed_ms : r.clientMs;
    $("lookup-timing").textContent = "Answered in " +
      (ms < 1000 ? Math.round(ms) + "ms" : (ms / 1000).toFixed(1) + "s");

    // Structured results when the payload carries them, the verbatim block
    // otherwise. Today it is always the block: the structured array is H3.
    var hits = r.hits && r.hits.length ? r.hits : null;

    show($("fallback-toggle"), !!hits);
    $("fallback-toggle").textContent = state.showFallback
      ? "Show results as cards"
      : "Show what the agent receives";

    var useCards = hits && !state.showFallback;
    show($("lookup-cards"), !!useCards);
    show($("lookup-fallback"), !useCards);

    if (!useCards) {
      show($("traversal-banner"), false);
      $("lookup-fallback").textContent = r.context || "(no answer)";
      return;
    }

    renderCards(hits);
  }

  function renderCards(hits) {
    var banner = $("traversal-banner");
    var traversal = null;
    var wrap = $("lookup-cards");
    clear(wrap);

    hits.forEach(function (h) {
      var anns = h.annotations || [];
      anns.forEach(function (a) { if (a.kind === "traversal") traversal = a.text; });

      var isPillar = anns.some(function (a) { return a.kind === "load_bearing"; });
      var card = el("div", "card" + (isPillar ? " is-pillar" : ""));

      var top = el("div", "card-top");
      top.appendChild(el("span", "card-content", h.content));
      if (h.concept_type) top.appendChild(kindBadge(h.concept_type));
      var sb = statusBadge(h.status);
      if (sb) top.appendChild(sb);
      card.appendChild(top);

      var scoreRow = el("div", "card-score");
      var track = el("div", "score-track");
      var fill = el("div", "score-fill");
      fill.style.width = Math.max(0, Math.min(100, (h.score || 0) * 100)) + "%";
      track.appendChild(fill);
      scoreRow.appendChild(track);
      scoreRow.appendChild(el("span", "muted-small", "Score " + (h.score || 0).toFixed(2)));
      if (h.blast_radius) {
        scoreRow.appendChild(el("span", "muted-small", "· " + h.blast_radius + " depend on it"));
      }
      card.appendChild(scoreRow);

      anns.filter(function (a) { return a.kind !== "traversal"; }).forEach(function (a) {
        var box = el("div", "annotation a-" + a.kind);
        box.appendChild(el("span", "annotation-label", a.kind.replace(/_/g, " ")));
        box.appendChild(el("span", null, a.text));
        card.appendChild(box);
      });

      wrap.appendChild(card);
    });

    show(banner, !!traversal);
    if (traversal) banner.textContent = traversal;
  }

  // ---- polling ---------------------------------------------------------

  function setConn(kind, label) {
    var c = $("conn");
    c.className = "conn is-" + kind;
    $("conn-label").textContent = label;
  }

  function poll() {
    return get("/api/pulse?since=" + state.seen)
      .then(function (p) {
        state.failures = 0;
        setConn("live", "Live");
        renderCounts(p.stats);
        var fresh = p.events && p.events.events && p.events.events.length;
        if (fresh) {
          state.events = state.events.concat(p.events.events);
          state.seen = p.events.total;
        }
        // Paint on the first answer even when there is nothing in it, so a
        // genuinely empty session says so; after that, only when something
        // actually moved, so the page does not redraw under the cursor.
        if (fresh || !state.painted) {
          state.painted = true;
          renderHistory();
          renderLadder();
        }
      })
      .catch(function (e) {
        state.failures++;
        setConn("stale", "Not connected (" + state.failures + ")");
      });
  }

  function schedule() {
    setTimeout(function () { poll().then(schedule); }, state.pollMs);
  }

  // The structure is not static on a session that is still being written to,
  // so it is refreshed, just far less often than the counts (H4).
  function loadGraph() {
    return get("/api/graph")
      .then(function (g) {
        state.graph = g;
        show($("structure"), true);
        buildTree();
        renderHero();
        renderLadder();
      })
      .catch(function () {
        // The endpoint is not available. The panel stays hidden rather than
        // drawing a structure this build cannot see.
        state.graph = null;
        show($("structure"), false);
        renderHero();
      });
  }

  // ---- boot ------------------------------------------------------------

  function boot() {
    initTheme();
    initLegendAndStatic();
    initLookup();
    $("details-clear").addEventListener("click", clearFocus);

    get("/api/session").then(renderSession).catch(function () {
      $("session-name").textContent = "unavailable";
    });

    loadGraph().then(function () {
      setInterval(loadGraph, 20000);
      try {
        var focus = new URLSearchParams(window.location.search).get("focus");
        if (focus) setFocus(focus);
      } catch (e) { /* ignore */ }
    });

    poll().then(schedule);
  }

  // The legend is the only thing that can be drawn before any data arrives.
  // The ladder and the history are deliberately NOT painted here: rendering
  // them empty would assert "nothing has happened in this session" during the
  // moment before the first response lands, which is a claim, not a spinner.
  // They appear when there is something true to say.
  function initLegendAndStatic() {
    renderLegend();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
