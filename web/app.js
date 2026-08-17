/* Lambo session window (T8.5).
 *
 * Read-only by construction: every request below is a GET against /api/*, and
 * the server exposes no mutating route. Nothing on this page can derive,
 * record an action, or reserve a node.
 *
 * Server text (the recall context block, concept content) is written with
 * .textContent, never innerHTML. The context block is agent-authored memory
 * and is rendered as the literal characters the agent would have received. */

(function () {
  "use strict";

  var POLL_MS = 1500;          // replaced from /api/session
  var pollTimer = null;
  var seen = 0;                // canonization events already rendered
  var lastCounts = {};
  var failures = 0;

  var el = {
    session:  document.getElementById("session-id"),
    chips:    document.getElementById("backend-chips"),
    banners:  document.getElementById("banners"),
    tiles:    document.getElementById("stat-tiles"),
    statsNote:document.getElementById("stats-note"),
    list:     document.getElementById("event-list"),
    empty:    document.getElementById("event-empty"),
    count:    document.getElementById("event-count"),
    form:     document.getElementById("recall-form"),
    query:    document.getElementById("recall-query"),
    go:       document.getElementById("recall-go"),
    meta:     document.getElementById("recall-meta"),
    out:      document.getElementById("recall-out"),
    status:   document.getElementById("link-status"),
    version:  document.getElementById("version"),
    factStore:    document.getElementById("fact-store"),
    factEmbedder: document.getElementById("fact-embedder"),
    factSearch:   document.getElementById("fact-search"),
    strip:        document.getElementById("livestrip"),
    liveHead:     document.getElementById("live-headline"),
    liveDetail:   document.getElementById("live-detail")
  };

  function get(path) {
    return fetch(path, { headers: { accept: "application/json" } }).then(function (r) {
      return r.json().then(function (body) {
        if (!r.ok) {
          throw new Error((body && body.error) || ("HTTP " + r.status));
        }
        return body;
      }, function () {
        throw new Error("HTTP " + r.status + " (unreadable body)");
      });
    });
  }

  function text(tag, cls, value) {
    var n = document.createElement(tag);
    if (cls) { n.className = cls; }
    if (value !== undefined && value !== null) { n.textContent = String(value); }
    return n;
  }

  // ---- banners -------------------------------------------------------

  function banner(message, hint) {
    var b = text("div", "banner");
    b.appendChild(text("span", "glyph", "⚑"));
    var body = text("div", null);
    body.appendChild(text("strong", null, message));
    if (hint) {
      body.appendChild(document.createTextNode(" "));
      body.appendChild(text("span", null, hint));
    }
    b.appendChild(body);
    el.banners.appendChild(b);
  }

  // ---- session identity ----------------------------------------------

  function applySession(info) {
    document.title = "lambo session " + info.session;
    el.session.textContent = info.session;
    el.version.textContent = "lambo " + info.version;

    // Same facts the chips used to carry, written so they can be read cold.
    var STORE_NAMES = { cockroach: "CockroachDB", sqlite: "SQLite", memory: "memory (this process)" };
    var MODEL_NAMES = { bge_m3: "BGE-M3", bedrock: "Amazon Titan", fixture: "fixture (test only)" };
    if (el.factStore) { el.factStore.textContent = STORE_NAMES[info.store] || info.store; }
    if (el.factEmbedder) {
      el.factEmbedder.textContent =
        (MODEL_NAMES[info.embedder] || info.embedder) + ", " + info.embedding_dim + " dimensions";
    }
    if (el.factSearch) {
      el.factSearch.textContent = info.vector_search
        ? "meaning, keyword and structure"
        : "keyword and structure";
    }

    el.chips.textContent = "";
    el.chips.appendChild(text("span", "chip", info.read_only ? "reader process, holds no write lease" : "writer"));

    if (info.poll_interval_ms) { POLL_MS = info.poll_interval_ms; }

    if (info.store_is_process_local) {
      banner(
        "The in-RAM store is process-local.",
        "This reader has its own empty copy, so it cannot see writes made by another " +
        "process. Point both at a sqlite or cockroach store to watch the session move."
      );
    }
    if (info.exposed_beyond_loopback) {
      banner(
        "Bound beyond loopback; bearer token required.",
        "Every request must send 'Authorization: Bearer <token>' (set via LAMBO_AUTH_TOKEN " +
        "or --auth-token). This surface stays read-only. If this page loaded without a " +
        "token, route it through an authenticating proxy from the start."
      );
    }
  }

  // ---- stats ----------------------------------------------------------

  // "nodes" and "edges" are what the graph calls them and mean nothing to a
  // reader arriving cold, so each tile says what it actually counts.
  var TILES = [
    { key: "concepts",  label: "things remembered", hint: "distinct facts, resources and rules" },
    { key: "nodes",     label: "records",           hint: "those, plus every action and interaction" },
    { key: "edges",     label: "connections",       hint: "recorded links between records" },
    { key: "canonical", label: "load-bearing",      hint: "proven enough that changing them is risky" },
    { key: "canonization_events", label: "status changes", hint: "promotions and demotions recorded" }
  ];

  function applyStats(s) {
    el.tiles.textContent = "";
    TILES.forEach(function (t) {
      var v = s[t.key];
      var tile = text("div", "tile" + (lastCounts[t.key] !== undefined && lastCounts[t.key] !== v ? " bumped" : ""));
      tile.appendChild(text("span", "k", t.label));
      tile.appendChild(text("span", "v", v));
      if (t.hint) { tile.appendChild(text("span", "h", t.hint)); }
      tile.setAttribute("data-key", t.key);   // lets CSS weight the tiles that matter
      el.tiles.appendChild(tile);
      lastCounts[t.key] = v;
    });

    // Flush stats come from the writer's FlushTask, published into the shared
    // store (T85-3). When a live writer has published them, show the real
    // numbers; when absent (no writer yet / store without support) show the
    // honest n/a with a writer-only tooltip, never a fabricated 0.
    // Flush lag and log depth are writer diagnostics. They were tiles, which gave
    // them the same visual weight as the numbers the page is actually about.
    var hasFlush = s.flush_lag_ms !== null && s.flush_lag_ms !== undefined;
    var age = Math.round((s.durable_change_age_ms || 0) / 1000);
    el.statsNote.textContent =
      "Last change seen " + age + "s ago. " +
      (hasFlush
        ? "The writing process reports " + s.flush_lag_ms + "ms behind, with " + s.log_depth + " pending."
        : "Write timings are only visible to the writing process, so they are not shown here.");
  }

  // ---- canonization feed ----------------------------------------------

  function clock(iso) {
    var d = new Date(iso);
    if (isNaN(d.getTime())) { return iso; }
    return d.toLocaleTimeString([], { hour12: false });
  }

  // The API publishes transitions, not a status roll-call, so the current
  // population of each rung is replayed from the event stream. Keyed by node id
  // rather than content: content can repeat, node identity cannot.
  var statusBy = {};
  var RUNGS = ["Candidate", "Venerable", "Canonical"];

  function renderLadder() {
    var buckets = { Candidate: [], Venerable: [], Canonical: [] };
    Object.keys(statusBy).forEach(function (id) {
      var e = statusBy[id];
      if (buckets[e.status]) { buckets[e.status].push(e); }
    });

    RUNGS.forEach(function (r) {
      var countEl = document.getElementById("n-" + r);
      var listEl = document.getElementById("list-" + r);
      if (!countEl || !listEl) { return; }

      var items = buckets[r].sort(function (a, b) {
        return (b.blast || 0) - (a.blast || 0) || a.name.localeCompare(b.name);
      });
      countEl.textContent = items.length;
      listEl.textContent = "";

      items.forEach(function (e) {
        var li = document.createElement("li");
        li.appendChild(text("span", "rl-name", e.name));
        if (e.blast !== null && e.blast !== undefined) {
          li.appendChild(text("span", "rl-blast", e.blast + " depend on it"));
        }
        listEl.appendChild(li);
      });
      if (!items.length) {
        listEl.appendChild(text("li", "rl-none", "none yet"));
      }
    });
  }

  function appendEvents(payload, animate) {
    if (payload.total < seen) {   // session reset underneath us
      el.list.textContent = "";
      statusBy = {};
      seen = 0;
    }
    payload.events.forEach(function (ev) {
      statusBy[ev.node_id] = {
        name: ev.content === null ? "(node " + ev.node_id.slice(0, 8) + ")" : ev.content,
        status: ev.to_status,
        blast: ev.blast_radius
      };

      var li = document.createElement("li");
      if (animate) { li.className = "fresh"; }
      li.appendChild(text("span", "at", clock(ev.occurred_at)));
      li.appendChild(text("span", "what", ev.content === null ? "(node " + ev.node_id.slice(0, 8) + ")" : ev.content));

      var demotion = ev.to_status === "None" && ev.from_status !== "None";
      var cls = "move" + (ev.to_status === "Canonical" ? " to-canonical" : demotion ? " demotion" : "");
      var move = text("span", cls, ev.from_status + " → " + ev.to_status);
      if (ev.blast_radius !== null && ev.blast_radius !== undefined) {
        move.appendChild(text("span", "radius", "   blast radius " + ev.blast_radius));
      }
      li.appendChild(move);
      el.list.insertBefore(li, el.list.firstChild);
    });
    seen = payload.total;
    el.count.textContent = seen;
    el.empty.className = seen > 0 ? "empty hidden" : "empty";
    renderLadder();
  }

  // ---- polling ---------------------------------------------------------

  var STRIP = {
    live:    ["Live", "reading this session as it changes"],
    stale:   ["Reconnecting", "the last read did not come back"],
    dead:    ["Not connected", "this page is not reading anything right now"]
  };

  function link(state, detail) {
    el.status.className = "status " + state;
    el.status.textContent = detail;

    // Said once, at the top, in words. The footer line was the only signal that
    // the page was live and it was the least prominent thing on screen.
    if (!el.strip) { return; }
    var copy = STRIP[state] || STRIP.dead;
    el.strip.className = "livestrip " + state;
    el.liveHead.textContent = copy[0];
    el.liveDetail.textContent =
      copy[1] + (state === "live" ? ", checking every " + (POLL_MS / 1000).toFixed(1) + " seconds" : "");
  }

  function poll(animate) {
    return get("/api/pulse?since=" + seen).then(function (p) {
      applyStats(p.stats);
      appendEvents(p.events, animate);
      failures = 0;
      link("live", "live · polling every " + (POLL_MS / 1000).toFixed(1) + "s");
    }).catch(function (e) {
      failures += 1;
      link("stale", "poll failed (" + failures + "): " + e.message);
    });
  }

  function schedule() {
    if (pollTimer) { clearTimeout(pollTimer); }
    pollTimer = setTimeout(function () {
      poll(true).then(schedule);
    }, POLL_MS);
  }

  // ---- structure -------------------------------------------------------

  // E2E-R1-1: this pane is driven by /api/graph (the whole session's structural
  // skeleton). /api/inspect additionally carries `gate_progress` for a focused
  // concept, but it is surfaced via the API only and is NOT called here. The
  // focus-driven detail panel — tree-node click → /api/inspect → dependents
  // list + the four canonization gates — is part of the parked UI pass (see
  // dev-diary/notes/ui-pass-plan.md, "Rough order" items 2-3), so it
  // intentionally does not render yet. gate_progress stays correct and tested
  // at the API level; the panel render lands when the UI pass resumes.

  // Built from /api/graph when the build serving this page has it. On any
  // failure the panel stays hidden: showing a placeholder tree would mean the
  // page is describing infrastructure it cannot see.
  function loadGraph() {
    var panel = document.getElementById("tree-panel");
    var out = document.getElementById("tree-out");
    if (!panel || !out) { return; }

    get("/api/graph").then(function (g) {
      var byName = {};
      (g.nodes || []).forEach(function (n) { byName[n.content] = n; });

      var children = {};
      var hasParent = {};
      (g.edges || []).forEach(function (e) {
        (children[e.parent] = children[e.parent] || []).push(e);
        hasParent[e.child] = true;
      });

      var roots = Object.keys(children)
        .filter(function (n) { return !hasParent[n]; })
        .sort();
      if (!roots.length) { return; }

      out.textContent = "";
      roots.forEach(function (r) { out.appendChild(branch(r, children, byName, 0)); });
      panel.className = "panel tree";
    }).catch(function () {
      // Endpoint absent on this build. Leave the panel hidden.
    });
  }

  function branch(name, children, byName, depth) {
    var node = byName[name] || {};
    var li = text("div", "tnode depth-" + Math.min(depth, 4));

    var row = text("div", "tnode-row");
    row.appendChild(text("span", "tnode-name", name));
    if (node.status === "Canonical") {
      row.appendChild(text("span", "tnode-tag", "load-bearing"));
    }
    if (node.blast_radius) {
      row.appendChild(text("span", "tnode-blast", node.blast_radius + " depend on it"));
    }
    li.appendChild(row);

    (children[name] || [])
      .slice()
      .sort(function (a, b) { return a.child.localeCompare(b.child); })
      .forEach(function (e) {
        li.appendChild(branch(e.child, children, byName, depth + 1));
      });
    return li;
  }

  // ---- recall ----------------------------------------------------------

  // The block is agent-authored memory, so it is built node by node with
  // textContent and never innerHTML. Highlighting it means one element per
  // line, classed by what the line is, not string-templated markup.
  function renderContext(body) {
    el.out.textContent = "";
    if (!body || !body.length) {
      el.out.appendChild(text("div", "ctx-line", "(the session returned nothing for that)"));
      return;
    }
    body.split("\n").forEach(function (line) {
      if (!line.trim()) {
        el.out.appendChild(text("div", "ctx-gap"));
        return;
      }
      var cls = "ctx-line";
      if (line.trim().charAt(0) === "\u2691") {
        cls += " ctx-warn";           // the load-bearing warning, the payoff line
      } else if (line.indexOf(", canonical]") !== -1) {
        cls += " ctx-canonical";      // a memory that earned its status
      }
      var row = text("div", cls);
      // Split the score suffix off so it can recede without hiding it.
      var at = line.lastIndexOf(" (score");
      if (at === -1) {
        row.appendChild(text("span", "ctx-main", line));
      } else {
        row.appendChild(text("span", "ctx-main", line.slice(0, at)));
        row.appendChild(text("span", "ctx-score", line.slice(at + 1)));
      }
      el.out.appendChild(row);
    });
  }

  // A suggested query fills the box and runs it, so the first thing a reader
  // does is see a real answer rather than guess at the vocabulary.
  Array.prototype.forEach.call(document.querySelectorAll(".chip-q"), function (b) {
    b.addEventListener("click", function () {
      el.query.value = b.getAttribute("data-q") || "";
      el.form.dispatchEvent(new Event("submit", { cancelable: true }));
    });
  });

  el.form.addEventListener("submit", function (e) {
    e.preventDefault();
    var q = el.query.value.trim();
    if (!q) { return; }

    el.go.disabled = true;
    el.meta.textContent = "recalling…";
    el.out.className = "context waiting";

    get("/api/recall?q=" + encodeURIComponent(q)).then(function (r) {
      el.meta.textContent =
        "returned in " + r.elapsed_ms + " ms · " + r.context.length + " characters, verbatim";
      el.out.className = "context";
      renderContext(r.context);
    }).catch(function (err) {
      el.meta.textContent = "";
      el.out.className = "context failed";
      el.out.textContent = "recall failed: " + err.message;
    }).then(function () {
      el.go.disabled = false;
    });
  });

  // Synthetic Return from browser automation can bypass implicit form
  // submission, so drive submit explicitly on Enter. preventDefault stops
  // the browser's native implicit submit too, so the path fires exactly once.
  el.query.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.defaultPrevented) {
      e.preventDefault();
      el.form.requestSubmit();
    }
  });

  // ---- boot ------------------------------------------------------------

  get("/api/session").then(function (info) {
    applySession(info);
    el.out.className = "context idle";
    loadGraph();          // structure is static for the session, fetched once
    return poll(false);
  }).then(schedule).catch(function (e) {
    link("stale", "cannot reach the session: " + e.message);
  });
})();
