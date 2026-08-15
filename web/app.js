/* Lambo session window (T8.5).
 *
 * Read-only by construction: every request below is a GET against /api/*, and
 * the server exposes no mutating route. Nothing on this page can derive,
 * record an action, or reserve a node.
 *
 * Server text (the recall context block, concept content) is written with
 * .textContent, never innerHTML — the context block is agent-authored memory
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
    version:  document.getElementById("version")
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
    document.title = "lambo — " + info.session;
    el.session.textContent = info.session;
    el.version.textContent = "lambo " + info.version;

    el.chips.textContent = "";
    el.chips.appendChild(text("span", "chip", "store: " + info.store));
    el.chips.appendChild(text("span", "chip", "embedder: " + info.embedder + " / " + info.embedding_dim + "d"));
    el.chips.appendChild(text("span", "chip", info.vector_search ? "hybrid recall" : "keyword recall"));

    if (info.poll_interval_ms) { POLL_MS = info.poll_interval_ms; }

    if (info.store_is_process_local) {
      banner(
        "The in-RAM store is process-local.",
        "This reader has its own empty copy — it cannot see writes made by another " +
        "process. Point both at a sqlite or cockroach store to watch the session move."
      );
    }
    if (info.exposed_beyond_loopback) {
      banner(
        "Bound beyond loopback on an unauthenticated server.",
        "Anyone who can reach this port can read the session. Keep it behind a private " +
        "network until authentication lands."
      );
    }
  }

  // ---- stats ----------------------------------------------------------

  var TILES = [
    { key: "nodes",      label: "nodes" },
    { key: "edges",      label: "edges" },
    { key: "concepts",   label: "concepts" },
    { key: "canonical",  label: "canonical" },
    { key: "canonization_events", label: "transitions" }
  ];

  function applyStats(s) {
    el.tiles.textContent = "";
    TILES.forEach(function (t) {
      var v = s[t.key];
      var tile = text("div", "tile" + (lastCounts[t.key] !== undefined && lastCounts[t.key] !== v ? " bumped" : ""));
      tile.appendChild(text("span", "k", t.label));
      tile.appendChild(text("span", "v", v));
      el.tiles.appendChild(tile);
      lastCounts[t.key] = v;
    });

    // flush_lag / log_depth belong to the writer process. A reader that
    // printed 0 for them would be claiming a durability bound it cannot see.
    [["flush lag", s.flush_lag_ms], ["log depth", s.log_depth]].forEach(function (pair) {
      var tile = text("div", "tile na");
      tile.appendChild(text("span", "k", pair[0]));
      tile.appendChild(text("span", "v", pair[1] === null || pair[1] === undefined ? "n/a" : pair[1]));
      tile.title = s.writer_only || "";
      el.tiles.appendChild(tile);
    });

    var age = Math.round((s.durable_change_age_ms || 0) / 1000);
    el.statsNote.textContent =
      "Last durable change seen by this reader: " + age + "s ago. " +
      "Flush lag and log depth are writer-only — a reader process cannot observe them.";
  }

  // ---- canonization feed ----------------------------------------------

  function clock(iso) {
    var d = new Date(iso);
    if (isNaN(d.getTime())) { return iso; }
    return d.toLocaleTimeString([], { hour12: false });
  }

  function appendEvents(payload, animate) {
    if (payload.total < seen) {   // session reset underneath us
      el.list.textContent = "";
      seen = 0;
    }
    payload.events.forEach(function (ev) {
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
  }

  // ---- polling ---------------------------------------------------------

  function link(state, detail) {
    el.status.className = "status " + state;
    el.status.textContent = detail;
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

  // ---- recall ----------------------------------------------------------

  el.form.addEventListener("submit", function (e) {
    e.preventDefault();
    var q = el.query.value.trim();
    if (!q) { return; }

    el.go.disabled = true;
    el.meta.textContent = "recalling…";
    el.out.className = "context waiting";

    get("/api/recall?q=" + encodeURIComponent(q)).then(function (r) {
      el.meta.textContent = "recall · " + r.elapsed_ms + " ms · " + r.context.length + " chars, verbatim";
      el.out.className = "context";
      el.out.textContent = r.context.length ? r.context : "(the session returned an empty context block)";
    }).catch(function (err) {
      el.meta.textContent = "";
      el.out.className = "context failed";
      el.out.textContent = "recall failed: " + err.message;
    }).then(function () {
      el.go.disabled = false;
    });
  });

  // ---- boot ------------------------------------------------------------

  get("/api/session").then(function (info) {
    applySession(info);
    el.out.className = "context idle";
    return poll(false);
  }).then(schedule).catch(function (e) {
    link("stale", "cannot reach the session: " + e.message);
  });
})();
