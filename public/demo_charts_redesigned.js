(function () {
  const questionEl = document.getElementById("question");
  const runBtn = document.getElementById("runBtn");
  const clearBtn = document.getElementById("clearBtn");
  const statusEl = document.getElementById("status");
  const answerEl = document.getElementById("answer");
  const chartEl = document.getElementById("chart");
  const chartTypeEl = document.getElementById("chartType");
  const rowCountEl = document.getElementById("rowCount");
  const chartSourceEl = document.getElementById("chartSource");
  const metricSelect = document.getElementById("metricSelect");
  const intervalSelect = document.getElementById("intervalSelect");
  const chartModeSelect = document.getElementById("chartModeSelect");
  const resetChartBtn = document.getElementById("resetChartBtn");
  const resetChartBtn2 = document.getElementById("resetChartBtn2");
  const queriesEl = document.getElementById("queries");
  const provenanceEl = document.getElementById("provenance");
  const rawDebugEl = document.getElementById("rawDebug");
  const planStepsEl = document.getElementById("planSteps");
  const whySummaryEl = document.getElementById("whySummary");
  const copyDebugBtn = document.getElementById("copyDebugBtn");
  const copyFullDebugBtn = document.getElementById("copyFullDebugBtn");
  const exportCsvBtn = document.getElementById("exportCsvBtn");
  const exportJsonBtn = document.getElementById("exportJsonBtn");
  const exportMarkdownBtn = document.getElementById("exportMarkdownBtn");
  const authStatusEl = document.getElementById("authStatus");
  const loginForm = document.getElementById("loginForm");
  const loginUsername = document.getElementById("loginUsername");
  const loginPassword = document.getElementById("loginPassword");
  const logoutBtn = document.getElementById("logoutBtn");
  const historyListEl = document.getElementById("historyList");
  const historyDetailEl = document.getElementById("historyDetail");
  const refreshHistoryBtn = document.getElementById("refreshHistoryBtn");
  const historySearchEl = document.getElementById("historySearch");
  const historySearchBtn = document.getElementById("historySearchBtn");
  const followupContextCard = document.getElementById("followupContextCard");
  const followupContextText = document.getElementById("followupContextText");
  const followupContextBadge = document.getElementById("followupContextBadge");
  const applyContextBtn = document.getElementById("applyContextBtn");
  const clearContextBtn = document.getElementById("clearContextBtn");
  let lastProvenance = null;
  let lastChartRows = [];
  let lastSourceLabel = "Provenance only";
  let lastRawDebug = "";
  let lastFinalAnswer = "";
  let followupContext = null;
  let isAdmin = false;
  let tablePage = 0;
  let resizeTimer = null;

  function setStatus(text, type = "idle") {
    statusEl.textContent = text;
    statusEl.className = "status-value";
    if (type === "success") statusEl.classList.add("success");
    if (type === "error") statusEl.classList.add("error");
  }

  function setRunning(running) {
    runBtn.disabled = running;
    clearBtn.disabled = running;
    if (running) {
      runBtn.innerHTML = '<div class="spinner"></div> Running...';
      setStatus("Running");
    } else {
      runBtn.innerHTML = '<span>▶</span> Run Query';
    }
  }

  function showEmpty(message, isError = false) {
    chartEl.innerHTML = "";
    const empty = document.createElement("div");
    empty.className = "chart-empty";
    empty.innerHTML = `
      <div class="chart-empty-icon">${isError ? "⚠️" : "📈"}</div>
      <div class="chart-empty-title">${isError ? "Error" : "No chart data"}</div>
      <div class="chart-empty-desc">${message}</div>
    `;
    chartEl.appendChild(empty);
  }

  function escapeHtml(value) {
    return String(value ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function parseSseEvent(raw) {
    const event = { event: "message", data: "" };
    for (const line of raw.split(/\r?\n/)) {
      if (!line || line.startsWith(":")) continue;
      const separator = line.indexOf(":");
      const field = separator === -1 ? line : line.slice(0, separator);
      const value = separator === -1 ? "" : line.slice(separator + 1).replace(/^ /, "");
      if (field === "event") {
        event.event = value || "message";
      } else if (field === "data") {
        event.data += event.data ? `\n${value}` : value;
      }
    }
    if (!event.data) return null;
    try {
      event.payload = JSON.parse(event.data);
    } catch (_err) {
      event.payload = { message: event.data };
    }
    return event;
  }

  async function readSseResponse(response, onEvent) {
    if (!response.ok || !response.body) {
      const text = await response.text().catch(() => "");
      let message = text || `Request failed with HTTP ${response.status}`;
      try {
        const parsed = JSON.parse(text);
        message = parsed.error || message;
      } catch (_err) {}
      if (response.status === 403) message = "Admin login required for debug features.";
      throw new Error(message);
    }

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let boundary = buffer.indexOf("\n\n");
      while (boundary !== -1) {
        const rawEvent = buffer.slice(0, boundary).trim();
        buffer = buffer.slice(boundary + 2);
        const event = parseSseEvent(rawEvent);
        if (event) onEvent(event);
        boundary = buffer.indexOf("\n\n");
      }
    }
    const tail = buffer.trim();
    if (tail) {
      const event = parseSseEvent(tail);
      if (event) onEvent(event);
    }
  }

  function prettyLabel(field) {
    const leaf = String(field || "").split(".").pop() || "";
    return leaf
      .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
      .replace(/_/g, " ")
      .trim() || field;
  }

  function formatValue(field, value) {
    if (value == null || value === "") return "-";
    if (isTemporalValue(value)) return new Date(value).toISOString().slice(0, 10);
    if (isFiniteNumber(value)) {
      const n = toNumber(value);
      const lower = String(field || "").toLowerCase();
      const digits = Math.abs(n) >= 100 ? 0 : 2;
      const formatted = n.toLocaleString(undefined, {
        maximumFractionDigits: lower.includes("capacity") ? 2 : digits,
      });
      if (lower.includes("ratedcapacity")) return `${formatted} MW`;
      return formatted;
    }
    return String(value);
  }

  function downloadText(filename, mimeType, content) {
    const blob = new Blob([content], { type: mimeType });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    document.body.appendChild(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
  }

  function csvEscape(value) {
    const text = String(value ?? "");
    return /[",\n\r]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
  }

  function exportRowsCsv(rows) {
    if (!rows.length) return "";
    const fields = Array.from(
      rows.reduce((set, row) => {
        Object.keys(row).forEach((key) => set.add(key));
        return set;
      }, new Set()),
    );
    const lines = [fields.map(csvEscape).join(",")];
    for (const row of rows) {
      lines.push(fields.map((field) => csvEscape(row[field])).join(","));
    }
    return lines.join("\n");
  }

  function exportRowsMarkdown(rows) {
    const answer = lastFinalAnswer || "No final answer recorded.";
    const root = firstRoot(lastProvenance);
    const grounding = groundingLabel(lastProvenance);
    const uncertainty = uncertaintyLabel(lastProvenance);
    const lines = [
      "# Zephyr Query Result",
      "",
      "## Question",
      "",
      questionEl.value.trim() || "(no question captured)",
      "",
      "## Answer",
      "",
      answer,
      "",
      "## Run Context",
      "",
      `- Root: ${root}`,
      `- Grounding: ${grounding}`,
      `- Uncertainty: ${uncertainty}`,
      `- Rows: ${rows.length}`,
      "",
      "## Evidence",
      "",
    ];
    if (!rows.length) {
      lines.push("_No row-aligned evidence captured._");
      return lines.join("\n");
    }
    const fields = displayFields(rows[0]).slice(0, 8);
    lines.push(`| ${fields.map(prettyLabel).join(" | ")} |`);
    lines.push(`| ${fields.map(() => "---").join(" | ")} |`);
    for (const row of rows) {
      lines.push(`| ${fields.map((field) => String(formatValue(field, row[field])).replace(/\|/g, "\\|")).join(" | ")} |`);
    }
    return lines.join("\n");
  }

  function exportCurrent(format) {
    const rows = lastChartRows || [];
    if (!lastProvenance && !rows.length) {
      setStatus("Nothing to export", "error");
      return;
    }
    const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
    if (format === "csv") {
      downloadText(`zephyr-result-${timestamp}.csv`, "text/csv;charset=utf-8", exportRowsCsv(rows));
    } else if (format === "json") {
      downloadText(
        `zephyr-result-${timestamp}.json`,
        "application/json;charset=utf-8",
        JSON.stringify(
          {
            question: questionEl.value.trim(),
            answer: lastFinalAnswer,
            rows,
            provenance: lastProvenance,
          },
          null,
          2,
        ),
      );
    } else {
      downloadText(`zephyr-result-${timestamp}.md`, "text/markdown;charset=utf-8", exportRowsMarkdown(rows));
    }
    setStatus(`Exported ${format.toUpperCase()}`, "success");
  }

  function displayFields(row) {
    const preferred = [
      "name",
      "shortName",
      "plantId",
      "ratedCapacity",
      "commercialDateTimeOfOperation",
      "timeZone",
      "state",
      "locationLabel",
      "locationId",
      "status",
      "categoryDescription",
      "system",
      "count",
    ];
    const keys = Object.keys(row).filter((key) => row[key] !== null && row[key] !== undefined && row[key] !== "");
    const score = (key) => {
      const leaf = key.split(".").pop();
      const idx = preferred.indexOf(leaf);
      return idx === -1 ? 100 + key.length : idx;
    };
    return keys.sort((a, b) => score(a) - score(b) || a.localeCompare(b));
  }

  function primaryTitle(row) {
    return row.name || row.shortName || row.location || row["partOfOffshoreWindFarm.name"] || "Single evidence row";
  }

  function subtitle(row) {
    const parts = [];
    if (row.shortName && row.shortName !== row.name) parts.push(row.shortName);
    if (row.plantId) parts.push(row.plantId);
    if (row.status) parts.push(row.status);
    return parts.join(" · ");
  }

  function compactRowPreview(row, maxFields = 4) {
    if (!row || typeof row !== "object") return "";
    const fields = displayFields(row).slice(0, maxFields);
    return fields
      .map((field) => `${prettyLabel(field)}: ${formatValue(field, row[field])}`)
      .join(", ");
  }

  function compactEntityPreview(row) {
    if (!row || typeof row !== "object") return "";
    const title = primaryTitle(row);
    const sub = subtitle(row);
    if (title && title !== "Single evidence row") {
      return sub ? `${title} (${sub})` : title;
    }
    return compactRowPreview(row, 3);
  }

  function firstRoot(provenance) {
    const steps = Array.isArray(provenance?.plan_steps) ? provenance.plan_steps : [];
    return steps.find((step) => step?.root_field)?.root_field || provenance?.scope_used?.executed_roots?.[0] || "-";
  }

  function entityLabels(provenance) {
    const keys = provenance?.grounding_confidence?.grounded_entity_keys;
    if (!Array.isArray(keys)) return [];
    return keys
      .map((key) => key?.display_label || key?.matched_value || key?.stable_key_value)
      .filter(Boolean)
      .map(String);
  }

  function updateFollowupContext(provenance, answer) {
    if (!provenance) return;
    const root = firstRoot(provenance);
    const labels = entityLabels(provenance);
    const rows = evidenceRows(provenance);
    const firstRow = rows[0] || {};
    const fallbackLabel = firstRow.name || firstRow.shortName || firstRow.location || "";
    const selectedLabels = labels.length ? labels : fallbackLabel ? [fallbackLabel] : [];
    followupContext = {
      root,
      labels: selectedLabels.slice(0, 3),
      metricFields: numericFields(fieldStats(rows)).slice(0, 3),
      rowCount: provenance?.evidence?.row_count ?? rows.length,
      answer: String(answer || "").slice(0, 180),
    };
    renderFollowupContext();
  }

  function renderFollowupContext() {
    if (!followupContext) {
      followupContextCard.hidden = true;
      followupContextText.textContent = "No context captured yet.";
      return;
    }
    followupContextCard.hidden = false;
    followupContextBadge.textContent = `${followupContext.rowCount} row${followupContext.rowCount === 1 ? "" : "s"}`;
    const labelText = followupContext.labels.length ? followupContext.labels.join(", ") : "no grounded entity label";
    const metricText = followupContext.metricFields.length ? followupContext.metricFields.join(", ") : "no numeric metric";
    followupContextText.innerHTML = `
      Last answer used <strong>${escapeHtml(followupContext.root)}</strong>,
      labels <strong>${escapeHtml(labelText)}</strong>,
      metrics <strong>${escapeHtml(metricText)}</strong>.
    `;
  }

  function contextNote() {
    if (!followupContext) return "";
    const parts = [`Use previous context root ${followupContext.root}.`];
    if (followupContext.labels.length) {
      parts.push(`Relevant labels: ${followupContext.labels.join(", ")}.`);
    }
    if (followupContext.metricFields.length) {
      parts.push(`Relevant metrics: ${followupContext.metricFields.join(", ")}.`);
    }
    return parts.join(" ");
  }

  function applyFollowupContext() {
    const note = contextNote();
    if (!note) return;
    const current = questionEl.value.trim();
    if (current.includes("Use previous context root")) return;
    questionEl.value = current ? `${current}\n\n${note}` : note;
    questionEl.focus();
  }

  function planStepsText(provenance) {
    const steps = Array.isArray(provenance?.plan_steps) ? provenance.plan_steps : [];
    if (!steps.length) return "No PlanV2 steps in provenance.";
    return steps
      .map((step, index) => {
        const parts = [`${index + 1}. ${step.description || `${step.op || "step"} ${step.id || ""}`.trim()}`];
        if (step.root_field) parts.push(`root: ${step.root_field}`);
        if (Array.isArray(step.fields) && step.fields.length) parts.push(`fields: ${step.fields.join(", ")}`);
        if (step.filter) parts.push(`filter: ${JSON.stringify(step.filter)}`);
        if (step.order) parts.push(`order: ${JSON.stringify(step.order)}`);
        return parts.join("\n   ");
      })
      .join("\n\n");
  }

  function executedQueryCount(provenance) {
    return Array.isArray(provenance?.executed_queries) ? provenance.executed_queries.length : 0;
  }

  function groundingLabel(provenance) {
    const overall = provenance?.grounding_confidence?.overall;
    if (!overall || overall === "none") return "not needed";
    return String(overall).replace(/_/g, " ");
  }

  function uncertaintyLabel(provenance) {
    const overall = provenance?.uncertainty?.overall;
    if (!overall || overall === "none") return "none";
    return String(overall).replace(/_/g, " ");
  }

  function uncertaintySignals(provenance) {
    const signals = provenance?.uncertainty?.signals;
    return Array.isArray(signals) ? signals.filter((signal) => signal && typeof signal === "object") : [];
  }

  function renderUncertaintyPanel(provenance) {
    const overall = uncertaintyLabel(provenance);
    const signals = uncertaintySignals(provenance);
    if (!signals.length) {
      return `
        <div class="uncertainty-panel is-clear" id="uncertaintyPanel">
          <div class="uncertainty-title">✓ No extra uncertainty signals</div>
          <div class="uncertainty-item">
            <span class="uncertainty-severity">clear</span>
            <span>The answer did not report repair drift, weak retrieval, missing scope, or backend relation limitations.</span>
          </div>
        </div>
      `;
    }
    return `
      <div class="uncertainty-panel" id="uncertaintyPanel">
        <div class="uncertainty-title">⚠ Uncertainty: ${escapeHtml(overall)}</div>
        <div class="uncertainty-list">
          ${signals
            .map((signal) => {
              const severity = String(signal.severity || "info").toLowerCase();
              const message = signal.message || signal.kind || "Uncertainty signal";
              return `
                <div class="uncertainty-item">
                  <span class="uncertainty-severity ${escapeHtml(severity)}">${escapeHtml(severity)}</span>
                  <span>${escapeHtml(message)}</span>
                </div>
              `;
            })
            .join("")}
        </div>
      </div>
    `;
  }

  function renderWhyStrip(provenance, rows, sourceLabel) {
    const items = [
      ["Root", firstRoot(provenance)],
      ["Rows", rows.length],
      ["Grounding", groundingLabel(provenance)],
      ["Uncertainty", uncertaintyLabel(provenance)],
      ["Source", sourceLabel],
    ];
    return `
      <div class="why-strip">
        ${items
          .map(
            ([label, value]) => `
              <div class="why-strip-item">
                <div class="why-strip-label">${escapeHtml(label)}</div>
                <div class="why-strip-value">${escapeHtml(value)}</div>
              </div>
            `,
          )
          .join("")}
      </div>
    `;
  }

  function renderWhySummary(provenance) {
    const items = [
      ["Root", firstRoot(provenance)],
      ["Plan Steps", Array.isArray(provenance?.plan_steps) ? provenance.plan_steps.length : 0],
      ["Grounding", groundingLabel(provenance)],
      ["Uncertainty", uncertaintyLabel(provenance)],
      ["Queries", executedQueryCount(provenance)],
    ];
    whySummaryEl.innerHTML = items
      .map(
        ([label, value]) => `
          <div class="why-card">
            <div class="why-label">${escapeHtml(label)}</div>
            <div class="why-value">${escapeHtml(value)}</div>
          </div>
        `,
      )
      .join("");
    const panel = document.getElementById("uncertaintyPanel");
    if (panel) {
      panel.outerHTML = renderUncertaintyPanel(provenance);
    }
  }

  function resetWhySummary() {
    whySummaryEl.innerHTML = ["Root", "Plan Steps", "Grounding", "Uncertainty", "Queries"]
      .map(
        (label) => `
          <div class="why-card">
            <div class="why-label">${escapeHtml(label)}</div>
            <div class="why-value">-</div>
          </div>
        `,
      )
      .join("");
    const panel = document.getElementById("uncertaintyPanel");
    if (panel) {
      panel.outerHTML = renderUncertaintyPanel(null);
    }
  }

  function renderDetailCard(rows, provenance, sourceLabel) {
    const row = rows[0];
    const fields = displayFields(row).filter((field) => field !== "name").slice(0, 8);
    chartEl.innerHTML = `
      <section class="detail-view">
        <div class="detail-header">
          <div class="detail-title-block">
            <h3>${escapeHtml(primaryTitle(row))}</h3>
            <div class="detail-subtitle">${escapeHtml(subtitle(row) || "Entity detail lookup")}</div>
          </div>
          <span class="badge badge-accent">Detail card</span>
        </div>
        <div class="detail-grid">
          ${fields
            .map(
              (field) => `
                <div class="detail-field">
                  <div class="detail-field-label">${escapeHtml(prettyLabel(field))}</div>
                  <div class="detail-field-value">${escapeHtml(formatValue(field, row[field]))}</div>
                </div>
              `,
            )
            .join("")}
        </div>
        ${renderWhyStrip(provenance, rows, sourceLabel)}
      </section>
    `;
  }

  function renderEvidenceTable(rows, provenance, sourceLabel) {
    const nested = nestedArrayInfo(provenance);
    const pageSize = 12;
    const totalPages = Math.max(1, Math.ceil(rows.length / pageSize));
    tablePage = Math.min(Math.max(tablePage, 0), totalPages - 1);
    const start = tablePage * pageSize;
    const visibleRows = rows.slice(start, start + pageSize);
    const fieldSet = new Set();
    for (const row of rows) {
      displayFields(row)
        .slice(0, 8)
        .forEach((field) => fieldSet.add(field));
    }
    const fields = Array.from(fieldSet).slice(0, 8);
    const title = nested
      ? `${nested.count} ${humanRelationLabel(nested.relationKey, nested.count, provenance?.answer)}`
      : `${rows.length} row${rows.length === 1 ? "" : "s"}`;
    const parentFields = nested ? displayFields(nested.parentContext).slice(0, 4) : [];
    const rangeStart = rows.length ? start + 1 : 0;
    const rangeEnd = Math.min(start + pageSize, rows.length);
    chartEl.innerHTML = `
      <section style="width: 100%;">
        <div class="result-table-header">
          <div>
            <div class="result-table-kicker">Structured result</div>
            <h3>${escapeHtml(title)}</h3>
          </div>
          ${
            nested && parentFields.length
              ? `<div class="result-parent-context">
                  ${parentFields
                    .map(
                      (field) => `
                        <span>
                          <strong>${escapeHtml(prettyLabel(field))}</strong>
                          ${escapeHtml(formatValue(field, nested.parentContext[field]))}
                        </span>
                      `,
                    )
                    .join("")}
                </div>`
              : ""
          }
        </div>
        <div class="table-wrap">
          <table class="data-table">
            <thead>
              <tr>${fields.map((field) => `<th>${escapeHtml(prettyLabel(field))}</th>`).join("")}</tr>
            </thead>
            <tbody>
              ${visibleRows
                .map(
                  (row) => `
                    <tr>
                      ${fields.map((field) => `<td>${escapeHtml(formatValue(field, row[field]))}</td>`).join("")}
                    </tr>
                  `,
                )
                .join("")}
            </tbody>
          </table>
        </div>
        <div class="table-pagination">
          <div class="table-page-status">
            Showing ${rangeStart}-${rangeEnd} of ${rows.length}
          </div>
          <div class="table-page-actions">
            <button class="btn-secondary btn-sm" type="button" data-table-page="prev" ${tablePage === 0 ? "disabled" : ""}>Previous</button>
            <span>Page ${tablePage + 1} / ${totalPages}</span>
            <button class="btn-secondary btn-sm" type="button" data-table-page="next" ${tablePage >= totalPages - 1 ? "disabled" : ""}>Next</button>
          </div>
        </div>
        ${renderWhyStrip(provenance, rows, sourceLabel)}
      </section>
    `;
    chartEl.querySelectorAll("[data-table-page]").forEach((button) => {
      button.addEventListener("click", () => {
        tablePage += button.dataset.tablePage === "next" ? 1 : -1;
        renderEvidenceTable(rows, provenance, sourceLabel);
      });
    });
  }

  function extractNamedJsonBlock(text, label) {
    if (!text) return null;
    const marker = `${label}:\n\`\`\`json`;
    const start = text.indexOf(marker);
    if (start === -1) return null;
    const jsonStart = start + marker.length;
    const end = text.indexOf("\n\`\`\`", jsonStart);
    if (end === -1) return null;
    try {
      return JSON.parse(text.slice(jsonStart, end).trim());
    } catch (_err) {
      return null;
    }
  }

  function extractFinalAnswer(text) {
    if (!text) return "";
    const marker = "Final Answer:\n";
    const start = text.lastIndexOf(marker);
    if (start === -1) return text.trim();
    const answerStart = start + marker.length;
    const provenanceStart = text.indexOf("\n\nProvenance:", answerStart);
    const end = provenanceStart === -1 ? text.length : provenanceStart;
    return text.slice(answerStart, end).trim();
  }

  function conciseAnswerText(rawAnswer, provenance) {
    const answer = String(rawAnswer || "").trim();
    const rows = evidenceRows(provenance);
    const rowCount = provenance?.evidence?.row_count ?? rows.length;
    const nested = nestedArrayInfo(provenance);
    if (nested) {
      const label = humanRelationLabel(nested.relationKey, nested.count, answer);
      const visibleCount = Math.min(nested.rows.length, 12);
      const preview = nested.rows.slice(0, 5).map(compactEntityPreview).filter(Boolean);
      const suffix = nested.count > preview.length ? `, and ${nested.count - preview.length} more` : "";
      return preview.length
        ? `Found ${nested.count} ${label}: ${preview.join("; ")}${suffix}. Showing ${visibleCount} in the structured result view.`
        : `Found ${nested.count} ${label}. Showing ${visibleCount} in the structured result view.`;
    }
    const root = humanRootLabel(firstRoot(provenance), rows.length);
    if (!answer) return "No final answer was returned.";
    const foundMatch = answer.match(/^Found\s+(\d+)\s+result\(s\):/i);
    if (foundMatch && rows.length > 1) {
      const preview = rows.slice(0, 4).map(compactEntityPreview).filter(Boolean);
      const suffix = rows.length > preview.length ? `, and ${rows.length - preview.length} more` : "";
      return preview.length
        ? `Found ${foundMatch[1]} ${root}: ${preview.join("; ")}${suffix}.`
        : `Found ${foundMatch[1]} ${root}.`;
    }
    if (foundMatch && rows.length === 1) {
      const preview = compactRowPreview(rows[0], 6);
      return preview ? `Found 1 matching ${root}: ${preview}.` : `Found 1 matching ${root}.`;
    }
    if (rows.length > 1 && answer.length > 260) {
      return `${answer.slice(0, 220).trim()}...`;
    }
    if (!rows.length && rowCount === 0 && /no matching records/i.test(answer)) {
      return "No matching records were returned for this query.";
    }
    return answer;
  }

  function finalAnswerOrStreamingText(content) {
    const finalAnswer = extractFinalAnswer(content);
    if (finalAnswer) return finalAnswer;
    return content ? "Streaming response..." : "Waiting for response...";
  }

  function humanRootLabel(root, count = 2) {
    const plural = count !== 1;
    const labels = {
      queryOffshoreWindFarm: plural ? "offshore wind farms" : "offshore wind farm",
      queryOffshoreWindTurbine: plural ? "offshore wind turbines" : "offshore wind turbine",
      queryWeatherPrediction: plural ? "weather predictions" : "weather prediction",
      queryPowerPrediction: plural ? "power predictions" : "power prediction",
      queryTag: plural ? "tags" : "tag",
      queryVessel: plural ? "vessels" : "vessel",
      queryHistoricalAisVesselpos: plural ? "vessel positions" : "vessel position",
      queryHistoricalScadaAgg10min: plural ? "SCADA samples" : "SCADA sample",
      queryHistoricalAggEvent: plural ? "events" : "event",
    };
    return labels[root] || (plural ? "results" : "record");
  }

  function humanRelationLabel(relationKey, count = 2, answerText = "") {
    const fromAnswer = String(answerText || "").match(/^Found\s+\d+\s+(.+?)\s+record\(s\)/i);
    if (fromAnswer?.[1]) return count === 1 ? fromAnswer[1].replace(/s$/i, "") : `${fromAnswer[1]}s`;
    const leaf = String(relationKey || "record").split(".").pop();
    const cleaned = leaf.replace(/^has/, "").replace(/^query/, "") || leaf;
    const label = prettyLabel(cleaned).toLowerCase();
    return count === 1 ? label : `${label}s`;
  }

  function executedQueriesText(provenance) {
    const queries = Array.isArray(provenance?.executed_queries)
      ? provenance.executed_queries
      : [];
    if (!queries.length) return "No executed queries in provenance.";
    return queries
      .map((query, index) => {
        const title = query.title || `Query ${index + 1}`;
        return `${title}\n\n${query.query || ""}`;
      })
      .join("\n\n---\n\n");
  }

  function flattenRow(value, prefix = "", out = {}) {
    if (!value || typeof value !== "object" || Array.isArray(value)) return out;
    for (const [key, raw] of Object.entries(value)) {
      const path = prefix ? `${prefix}.${key}` : key;
      if (raw == null) {
        out[path] = raw;
      } else if (Array.isArray(raw)) {
        out[path] = raw.length;
      } else if (typeof raw === "object") {
        flattenRow(raw, path, out);
      } else {
        out[path] = raw;
      }
    }
    return out;
  }

  function nestedArrayInfo(provenance) {
    const sampleRows = provenance?.evidence?.sample_rows;
    if (!Array.isArray(sampleRows) || sampleRows.length !== 1) return null;
    const parent = sampleRows[0];
    if (!parent || typeof parent !== "object" || Array.isArray(parent)) return null;
    const candidates = Object.entries(parent)
      .filter(([, value]) => Array.isArray(value) && value.some((row) => row && typeof row === "object" && !Array.isArray(row)))
      .map(([key, value]) => ({ key, rows: value }));
    if (!candidates.length) return null;
    candidates.sort((a, b) => b.rows.length - a.rows.length || a.key.localeCompare(b.key));
    const selected = candidates[0];
    const parentContext = Object.fromEntries(
      Object.entries(parent).filter(([, value]) => value == null || typeof value !== "object"),
    );
    const rows = selected.rows
      .filter((row) => row && typeof row === "object" && !Array.isArray(row))
      .map((row) => flattenRow(row));
    return {
      relationKey: selected.key,
      rows,
      count: selected.rows.length,
      parentContext,
    };
  }

  function filterLabelFromStep(step) {
    const filter = step?.filter;
    if (!filter || typeof filter !== "object" || Array.isArray(filter)) return "";
    for (const [field, condition] of Object.entries(filter)) {
      if (!condition || typeof condition !== "object" || Array.isArray(condition)) continue;
      const value = condition.eq ?? condition.in?.[0];
      if (value !== null && value !== undefined && value !== "") {
        return String(value);
      }
      if (field && typeof condition === "string") return condition;
    }
    return "";
  }

  function compareLabelsFromPlan(provenance) {
    const steps = Array.isArray(provenance?.plan_steps) ? provenance.plan_steps : [];
    if (!steps.some((step) => step?.op === "compare")) return [];
    const byId = new Map(steps.filter((step) => step?.id).map((step) => [step.id, step]));
    const labels = [];
    for (const compareStep of steps.filter((step) => step?.op === "compare")) {
      for (const side of [compareStep.left, compareStep.right]) {
        let step = byId.get(side);
        if (step?.op === "aggregate" && step.source) step = byId.get(step.source);
        const label = filterLabelFromStep(step);
        if (label) labels.push(label);
      }
    }
    if (labels.length) return labels;
    return steps
      .filter((step) => step?.op === "fetch")
      .map(filterLabelFromStep)
      .filter(Boolean);
  }

  function addComparisonLabels(rows, provenance) {
    const stats = fieldStats(rows);
    if (preferredCategoryField(stats)) return rows;
    const labels = compareLabelsFromPlan(provenance);
    if (labels.length !== rows.length) return rows;
    return rows.map((row, index) => ({
      comparison: labels[index],
      ...row,
    }));
  }

  function evidenceRows(provenance) {
    const nested = nestedArrayInfo(provenance);
    if (nested) return addComparisonLabels(nested.rows, provenance);
    const rows = provenance?.evidence?.sample_rows;
    if (!Array.isArray(rows)) return [];
    const flattened = rows
      .filter((row) => row && typeof row === "object" && !Array.isArray(row))
      .map((row) => flattenRow(row));
    return addComparisonLabels(flattened, provenance);
  }

  function firstExecutedQuery(provenance) {
    const queries = Array.isArray(provenance?.executed_queries)
      ? provenance.executed_queries
      : [];
    return queries.find((query) => typeof query?.query === "string" && query.query.trim())?.query || "";
  }

  function rowArraysFromGraphqlValue(value, out = []) {
    if (Array.isArray(value)) {
      if (value.some((row) => row && typeof row === "object" && !Array.isArray(row))) {
        out.push(value);
      }
      for (const item of value) rowArraysFromGraphqlValue(item, out);
    } else if (value && typeof value === "object") {
      for (const child of Object.values(value)) rowArraysFromGraphqlValue(child, out);
    }
    return out;
  }

  function rowsFromGraphqlData(data) {
    if (!data || typeof data !== "object") return [];
    const topLevel = Object.values(data).filter(Array.isArray);
    const candidates = topLevel.length ? topLevel : rowArraysFromGraphqlValue(data);
    const rows = candidates
      .sort((a, b) => b.length - a.length)[0] || [];
    return rows
      .filter((row) => row && typeof row === "object" && !Array.isArray(row))
      .map((row) => flattenRow(row));
  }

  async function replayExecutedQueryRows(provenance) {
    const query = firstExecutedQuery(provenance);
    if (!query) return { rows: [], note: "No executed query was available to replay." };
    try {
      const response = await fetch("/graphql/query", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ query, variables: {} }),
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        const message = payload?.error || payload?.errors?.[0]?.message || `HTTP ${response.status}`;
        return { rows: [], note: `Replay unavailable: ${message}` };
      }
      if (Array.isArray(payload.errors) && payload.errors.length) {
        const message = payload.errors
          .map((error) => error?.message)
          .filter(Boolean)
          .join("; ");
        return { rows: [], note: `Replay returned GraphQL errors: ${message || "unknown error"}` };
      }
      return { rows: rowsFromGraphqlData(payload.data), note: "" };
    } catch (err) {
      return { rows: [], note: `Replay request failed: ${err.message}` };
    }
  }

  function isFiniteNumber(value) {
    if (typeof value === "number") return Number.isFinite(value);
    if (typeof value !== "string" || value.trim() === "") return false;
    return Number.isFinite(Number(value));
  }

  function toNumber(value) {
    return typeof value === "number" ? value : Number(value);
  }

  function isTemporalValue(value) {
    if (typeof value !== "string") return false;
    if (!/\d{4}-\d{2}-\d{2}/.test(value)) return false;
    return Number.isFinite(Date.parse(value));
  }

  function isTemporalField(field, values) {
    const lower = field.toLowerCase().split(".").pop() || "";
    return (
      lower === "time" ||
      lower === "date" ||
      lower === "timestamp" ||
      lower.endsWith("date") ||
      lower.endsWith("datetime") ||
      lower.endsWith("timestamp") ||
      values.some(isTemporalValue)
    );
  }

  function distinctCount(values) {
    return new Set(values.map((value) => String(value))).size;
  }

  function fieldStats(rows) {
    const fields = Array.from(
      rows.reduce((set, row) => {
        Object.keys(row).forEach((key) => set.add(key));
        return set;
      }, new Set()),
    );
    return fields.map((field) => {
      const values = rows
        .map((row) => row[field])
        .filter((value) => value !== null && value !== undefined && value !== "");
      const numeric = values.length > 0 && values.every(isFiniteNumber);
      const temporal = values.length > 0 && isTemporalField(field, values);
      return {
        field,
        values,
        numeric,
        temporal,
        distinct: distinctCount(values),
      };
    });
  }

  function numericFields(stats) {
    return stats.filter((stat) => stat.numeric && !stat.temporal).map((stat) => stat.field);
  }

  function selectedMetricFields(stats) {
    const selected = metricSelect.value;
    if (!selected) return numericFields(stats).slice(0, 5);
    const stat = stats.find((candidate) => candidate.field === selected);
    return stat?.numeric && !stat.temporal ? [selected] : numericFields(stats).slice(0, 5);
  }

  function setMetricOptions(rows) {
    const previous = metricSelect.value;
    const fields = numericFields(fieldStats(rows));
    metricSelect.innerHTML = "";
    const auto = document.createElement("option");
    auto.value = "";
    auto.textContent = "Auto";
    metricSelect.appendChild(auto);
    for (const field of fields) {
      const option = document.createElement("option");
      option.value = field;
      option.textContent = field;
      metricSelect.appendChild(option);
    }
    if (fields.includes(previous)) metricSelect.value = previous;
  }

  async function rerenderCurrentChart() {
    if (!lastProvenance || !lastChartRows.length) return;
    await renderChartRows(lastChartRows, lastProvenance, lastSourceLabel);
  }

  function temporalField(stats) {
    return stats.find((stat) => stat.temporal)?.field || null;
  }

  function compareByTemporal(field) {
    return (a, b) => {
      const av = Date.parse(a[field]);
      const bv = Date.parse(b[field]);
      if (Number.isFinite(av) && Number.isFinite(bv)) return av - bv;
      return String(a[field] ?? "").localeCompare(String(b[field] ?? ""));
    };
  }

  function applyInterval(rows) {
    const value = intervalSelect.value;
    if (value === "all" || rows.length <= 1) return rows;
    const stats = fieldStats(rows);
    const timeField = temporalField(stats);
    const ordered = timeField ? [...rows].sort(compareByTemporal(timeField)) : [...rows];
    const [, rawCount] = value.split("-");
    const count = Number(rawCount);
    if (!Number.isFinite(count)) return rows;
    return value.startsWith("first") ? ordered.slice(0, count) : ordered.slice(-count);
  }

  function preferredCategoryField(stats) {
    const candidates = stats.filter((stat) => {
      if (!stat.values.length || stat.numeric || stat.temporal) return false;
      if (stat.distinct < 2) return false;
      return true;
    });
    const preferred = [
      "categoryDescription",
      "status",
      "system",
      "name",
      "shortName",
      "stringName",
      "location",
    ].map((name) => name.toLowerCase());
    candidates.sort((a, b) => {
      const ai = preferred.indexOf(a.field.toLowerCase());
      const bi = preferred.indexOf(b.field.toLowerCase());
      const ar = ai === -1 ? 99 : ai;
      const br = bi === -1 ? 99 : bi;
      return ar - br || b.distinct - a.distinct || a.field.localeCompare(b.field);
    });
    return candidates[0]?.field || null;
  }

  function preferredNumericField(stats) {
    const selected = metricSelect.value;
    if (selected) return selected;
    const candidates = stats.filter((stat) => stat.numeric && !stat.temporal);
    const preferred = ["count", "value", "avg", "sum", "ratedCapacity", "powerPrediction"];
    candidates.sort((a, b) => {
      const ai = preferred.findIndex((name) => a.field.toLowerCase().includes(name.toLowerCase()));
      const bi = preferred.findIndex((name) => b.field.toLowerCase().includes(name.toLowerCase()));
      const ar = ai === -1 ? 99 : ai;
      const br = bi === -1 ? 99 : bi;
      return ar - br || a.field.localeCompare(b.field);
    });
    return candidates[0]?.field || null;
  }

  function buildLineChart(rows, stats) {
    const temporal = stats.find((stat) => stat.temporal);
    if (!temporal) return null;
    const fields = selectedMetricFields(stats);
    if (!fields.length) return null;
    const values = [];
    for (const row of rows) {
      for (const field of fields) {
        if (isFiniteNumber(row[field]) && row[temporal.field] != null) {
          values.push({
            [temporal.field]: row[temporal.field],
            series: field,
            value: toNumber(row[field]),
          });
        }
      }
    }
    if (values.length < 2) return null;
    return {
      kind: "Line",
      spec: {
        $schema: "https://vega.github.io/schema/vega-lite/v5.json",
        autosize: { type: "fit-x", contains: "padding" },
        width: "container",
        height: 320,
        data: { values },
        params: [
          {
            name: "zoom",
            select: { type: "interval", encodings: ["x"] },
            bind: "scales",
          },
        ],
        mark: { type: "line", point: true, strokeWidth: 2.5 },
        encoding: {
          x: { field: temporal.field, type: "temporal", title: temporal.field },
          y: { field: "value", type: "quantitative", title: "Value" },
          color: { field: "series", type: "nominal", title: "Series" },
          tooltip: [
            { field: temporal.field, type: "temporal" },
            { field: "series", type: "nominal" },
            { field: "value", type: "quantitative" },
          ],
        },
      },
    };
  }

  function buildBarChart(rows, stats) {
    const category = preferredCategoryField(stats);
    const numeric = preferredNumericField(stats);
    if (!category || !numeric) return null;
    const values = rows
      .filter((row) => row[category] != null && isFiniteNumber(row[numeric]))
      .map((row) => ({
        [category]: row[category],
        [numeric]: toNumber(row[numeric]),
      }));
    if (values.length < 2) return null;
    return {
      kind: "Bar",
      spec: {
        $schema: "https://vega.github.io/schema/vega-lite/v5.json",
        autosize: { type: "fit-x", contains: "padding" },
        width: "container",
        height: Math.max(260, Math.min(420, values.length * 48)),
        data: { values },
        mark: { type: "bar", cornerRadiusEnd: 4 },
        encoding: {
          y: { field: category, type: "nominal", sort: "-x", title: category },
          x: { field: numeric, type: "quantitative", title: numeric },
          color: { value: "#38bda4" },
          tooltip: [
            { field: category, type: "nominal" },
            { field: numeric, type: "quantitative" },
          ],
        },
      },
    };
  }

  function buildScatterChart(rows, stats) {
    const selected = metricSelect.value;
    const fields = stats
      .filter((stat) => stat.numeric && !stat.temporal)
      .map((stat) => stat.field);
    if (fields.length < 2) return null;
    const selectedIndex = selected ? fields.indexOf(selected) : -1;
    const xField = selectedIndex > 0 ? fields[0] : fields[0];
    const yField = selectedIndex > 0 ? selected : fields[1];
    if (!xField || !yField || xField === yField) return null;
    const category = preferredCategoryField(stats);
    const values = rows
      .filter((row) => isFiniteNumber(row[xField]) && isFiniteNumber(row[yField]))
      .map((row) => ({
        [xField]: toNumber(row[xField]),
        [yField]: toNumber(row[yField]),
        label: category ? row[category] : undefined,
      }));
    if (values.length < 2) return null;
    const tooltip = [
      { field: xField, type: "quantitative" },
      { field: yField, type: "quantitative" },
    ];
    if (category) tooltip.push({ field: "label", type: "nominal" });
    return {
      kind: "Scatter",
      spec: {
        $schema: "https://vega.github.io/schema/vega-lite/v5.json",
        autosize: { type: "fit-x", contains: "padding" },
        width: "container",
        height: 320,
        data: { values },
        mark: { type: "point", filled: true, size: 90, opacity: 0.8 },
        encoding: {
          x: { field: xField, type: "quantitative" },
          y: { field: yField, type: "quantitative" },
          color: category ? { field: "label", type: "nominal" } : { value: "#38bda4" },
          tooltip,
        },
      },
    };
  }

  function fieldsFromOrder(order) {
    if (!order || typeof order !== "object" || Array.isArray(order)) return [];
    return Object.values(order).filter((value) => typeof value === "string");
  }

  function matchingStat(stats, field) {
    return stats.find((stat) => stat.field === field || stat.field.endsWith(`.${field}`));
  }

  function planPrefersBar(provenance, stats) {
    const steps = Array.isArray(provenance?.plan_steps) ? provenance.plan_steps : [];
    return steps.some((step) => {
      if (step?.op === "aggregate" || step?.op === "compare") return true;
      return fieldsFromOrder(step?.order).some((field) => {
        const stat = matchingStat(stats, field);
        return stat?.numeric && !stat.temporal;
      });
    });
  }

  function inferChart(rows, provenance) {
    if (rows.length < 2) {
      return {
        kind: "None",
        reason: "At least two row-aligned sample rows are required for charting.",
      };
    }
    if (chartModeSelect.value === "auto" && nestedArrayInfo(provenance)) {
      return {
        kind: "Table",
        reason: "Nested relation lists open as tables in auto mode.",
      };
    }
    const stats = fieldStats(rows);
    const mode = chartModeSelect.value;
    if (mode === "line") {
      return buildLineChart(rows, stats) || {
        kind: "None",
        reason: "Line mode needs a time field and a numeric metric.",
      };
    }
    if (mode === "bar") {
      return buildBarChart(rows, stats) || {
        kind: "None",
        reason: "Bar mode needs a category field and a numeric metric.",
      };
    }
    if (mode === "scatter") {
      return buildScatterChart(rows, stats) || {
        kind: "None",
        reason: "Scatter mode needs two numeric fields.",
      };
    }
    const bar = buildBarChart(rows, stats);
    if (bar && planPrefersBar(provenance, stats)) return bar;
    return (
      buildLineChart(rows, stats) ||
      bar ||
      buildScatterChart(rows, stats) || {
        kind: "None",
        reason: "The sample rows do not contain a supported field combination for charting.",
      }
    );
  }

  async function renderChartRows(rows, provenance, sourceLabel) {
    const filteredRows = applyInterval(rows);
    setMetricOptions(filteredRows);
    if (rows !== lastChartRows) tablePage = 0;
    const chart = inferChart(filteredRows, provenance);
    chartTypeEl.textContent = chart.kind;
    rowCountEl.textContent =
      filteredRows.length === rows.length ? String(rows.length) : `${filteredRows.length} / ${rows.length}`;
    chartSourceEl.textContent = sourceLabel;

    if (!chart.spec) {
      if (filteredRows.length === 1) {
        renderDetailCard(filteredRows, provenance, sourceLabel);
        chartTypeEl.textContent = "Detail";
        return true;
      }
      if (filteredRows.length > 1) {
        renderEvidenceTable(filteredRows, provenance, sourceLabel);
        chartTypeEl.textContent = "Table";
        return true;
      }
      showEmpty(chart.reason);
      return false;
    }
    if (typeof window.vegaEmbed !== "function") {
      showEmpty("Vega-Lite scripts did not load. Check network access for the CDN assets.", true);
      return false;
    }
    const spec = sizedSpec(chart.spec);
    chartEl.innerHTML = "";
    await window.vegaEmbed(chartEl, spec, { actions: false, renderer: "svg" });
    return true;
  }

  async function renderChart(provenance) {
    const rows = evidenceRows(provenance);
    lastChartRows = rows;
    lastSourceLabel = "Provenance sample_rows";
    const renderedFromProvenance = await renderChartRows(rows, provenance, "Provenance sample_rows");
    if (renderedFromProvenance || rows.length >= 2 || rows.length === 1) return;

    chartSourceEl.textContent = "Replaying executed query...";
    const replay = await replayExecutedQueryRows(provenance);
    if (replay.rows.length >= 1) {
      lastChartRows = replay.rows;
      lastSourceLabel = "Replayed executed query";
      const renderedFromReplay = await renderChartRows(replay.rows, provenance, "Replayed executed query");
      if (renderedFromReplay) return;
    }

    chartTypeEl.textContent = "None";
    rowCountEl.textContent = replay.rows.length ? String(replay.rows.length) : String(rows.length);
    chartSourceEl.textContent = replay.rows.length ? "Replayed executed query" : "Provenance only";
    const base = "At least two row-aligned sample rows are required for charting.";
    showEmpty(replay.note ? `${base} ${replay.note}` : base);
  }

  function chartWidth() {
    const host = chartEl.closest(".panel") || chartEl.parentElement || chartEl;
    const rect = host.getBoundingClientRect();
    return Math.max(360, Math.floor(rect.width) - 84);
  }

  function sizedSpec(spec) {
    const next = JSON.parse(JSON.stringify(spec));
    next.width = chartWidth();
    next.autosize = { type: "fit", contains: "padding", resize: true };
    next.background = "transparent";
    next.config = {
      ...(next.config || {}),
      view: { ...(next.config?.view || {}), stroke: "transparent" },
      axis: {
        ...(next.config?.axis || {}),
        domainColor: "rgba(148,163,184,0.28)",
        gridColor: "rgba(148,163,184,0.12)",
        labelColor: "#cbd5e1",
        labelFont: "Inter, system-ui, sans-serif",
        labelFontSize: 12,
        titleColor: "#f0f4f8",
        titleFont: "Inter, system-ui, sans-serif",
        titleFontSize: 12,
        titleFontWeight: 600,
        tickColor: "rgba(148,163,184,0.28)",
      },
      legend: {
        ...(next.config?.legend || {}),
        labelColor: "#cbd5e1",
        titleColor: "#f0f4f8",
        labelFont: "Inter, system-ui, sans-serif",
        titleFont: "Inter, system-ui, sans-serif",
      },
    };
    return next;
  }

  function resetOutputs() {
    lastProvenance = null;
    lastChartRows = [];
    lastSourceLabel = "Provenance only";
    lastRawDebug = "";
    lastFinalAnswer = "";
    tablePage = 0;
    metricSelect.innerHTML = '<option value="">Auto</option>';
    intervalSelect.value = "all";
    chartModeSelect.value = "auto";
    setStatus("Idle");
    answerEl.className = "answer-box empty";
    answerEl.style.color = "";
    answerEl.textContent = "Run a question to show the grounded answer.";
    chartEl.innerHTML = `
      <div class="chart-empty">
        <div class="chart-empty-icon">📈</div>
        <div class="chart-empty-title">No chart data</div>
        <div class="chart-empty-desc">Charts appear when Provenance.evidence.sample_rows contains at least two row-aligned, chartable records.</div>
      </div>
    `;
    chartTypeEl.textContent = "-";
    rowCountEl.textContent = "-";
    chartSourceEl.textContent = "Provenance only";
    queriesEl.textContent = "No query yet.";
    provenanceEl.textContent = "No provenance yet.";
    planStepsEl.textContent = "No plan yet.";
    rawDebugEl.textContent = "No full debug output yet.";
    resetWhySummary();
  }

  function applyAuthState(session) {
    isAdmin = !!session?.authenticated && session?.role === "admin";
    document.querySelectorAll(".admin-only").forEach((node) => {
      node.hidden = !isAdmin;
    });
    if (loginForm) {
      loginForm.classList.toggle("visible", !isAdmin && !!session?.admin_configured);
    }
    if (isAdmin) {
      authStatusEl.textContent = "Admin unlocked";
      authStatusEl.className = "badge badge-accent";
    } else {
      authStatusEl.textContent = session?.admin_configured ? "Public mode" : "Public mode";
      authStatusEl.className = "badge badge-info";
    }
  }

  async function checkSession() {
    try {
      const response = await fetch("/auth/session");
      const session = await response.json().catch(() => ({}));
      applyAuthState(session);
      loadHistory().catch(() => {});
      return isAdmin;
    } catch (_err) {
      authStatusEl.textContent = "Session unavailable";
      authStatusEl.className = "badge badge-danger";
      return false;
    }
  }

  async function login(event) {
    event.preventDefault();
    const username = loginUsername.value.trim() || "admin";
    const password = loginPassword.value;
    try {
      const response = await fetch("/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password }),
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(payload.error || "Login failed.");
      loginPassword.value = "";
      applyAuthState(payload);
      setStatus("Admin unlocked", "success");
      loadHistory().catch(() => {});
    } catch (err) {
      setStatus(err.message || "Login failed", "error");
    }
  }

  async function logout() {
    try {
      await fetch("/auth/logout", { method: "POST" });
    } finally {
      applyAuthState({ authenticated: false, role: "anonymous", admin_configured: true });
      resetOutputs();
      setStatus("Public mode", "success");
      loadHistory().catch(() => {});
    }
  }

  function formatTimestamp(value) {
    if (!value) return "unknown time";
    const date = new Date(value);
    if (!Number.isFinite(date.getTime())) return String(value);
    return date.toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function renderHistoryDetail(entry) {
    const rawAnswer = entry.answer || entry.error || "";
    const provenance = extractNamedJsonBlock(rawAnswer, "Provenance");
    const finalAnswer = extractFinalAnswer(rawAnswer) || rawAnswer || "(no answer recorded)";
    const rows = provenance ? evidenceRows(provenance) : [];
    const root = provenance ? firstRoot(provenance) : "-";
    const grounding = provenance ? groundingLabel(provenance) : "-";
    const uncertainty = provenance ? uncertaintyLabel(provenance) : "-";
    const meta = `${entry.success ? "Success" : "Failed"} · ${
      Number.isFinite(entry.execution_ms) ? entry.execution_ms : 0
    }ms · ${formatTimestamp(entry.timestamp)}`;
    historyDetailEl.innerHTML = `
      <div class="inspector-header">
        <div class="inspector-title">Run inspector</div>
        <span class="badge ${entry.success ? "badge-accent" : "badge-danger"}">${entry.success ? "Success" : "Failed"}</span>
      </div>
      <div class="history-inspector-grid">
        <div class="history-inspector-card">
          <div class="history-inspector-label">Root</div>
          <div class="history-inspector-value">${escapeHtml(root)}</div>
        </div>
        <div class="history-inspector-card">
          <div class="history-inspector-label">Rows</div>
          <div class="history-inspector-value">${escapeHtml(rows.length || provenance?.evidence?.row_count || 0)}</div>
        </div>
        <div class="history-inspector-card">
          <div class="history-inspector-label">Grounding</div>
          <div class="history-inspector-value">${escapeHtml(grounding)}</div>
        </div>
        <div class="history-inspector-card">
          <div class="history-inspector-label">Uncertainty</div>
          <div class="history-inspector-value">${escapeHtml(uncertainty)}</div>
        </div>
      </div>
      <div class="history-actions">
        <button class="btn-secondary btn-sm" type="button" data-history-action="rerun">Rerun</button>
        <button class="btn-secondary btn-sm" type="button" data-history-action="load-result">Load result view</button>
        <button class="btn-secondary btn-sm" type="button" data-history-action="copy-answer">Copy answer</button>
      </div>
      <div class="history-detail-grid">
        <section class="history-detail-block">
          <div class="history-detail-label">Question</div>
          <div class="history-detail-body">${escapeHtml(entry.question || "(no question)")}</div>
        </section>
        <section class="history-detail-block">
          <div class="history-detail-label">Answer</div>
          <div class="history-detail-body history-answer-preview">${escapeHtml(finalAnswer)}</div>
        </section>
        <section class="history-detail-block">
          <div class="history-detail-label">Run Metadata</div>
          <div class="history-detail-body">${escapeHtml(meta)}</div>
        </section>
        ${
          provenance
            ? `<section class="history-detail-block">
                <div class="history-detail-label">Grounding</div>
                <div class="history-detail-body">${escapeHtml(groundingLabel(provenance))}</div>
              </section>`
            : ""
        }
      </div>
    `;
    historyDetailEl.querySelector('[data-history-action="rerun"]')?.addEventListener("click", () => {
      questionEl.value = entry.question || "";
      runDemo();
    });
    historyDetailEl.querySelector('[data-history-action="load-result"]')?.addEventListener("click", async () => {
      if (!provenance) {
        setStatus("No provenance in selected run", "error");
        return;
      }
      lastProvenance = provenance;
      lastRawDebug = rawAnswer;
      lastFinalAnswer = finalAnswer;
      answerEl.className = "answer-box";
      answerEl.style.color = "";
      answerEl.textContent = conciseAnswerText(finalAnswer, provenance);
      provenanceEl.textContent = JSON.stringify(provenance, null, 2);
      queriesEl.textContent = executedQueriesText(provenance);
      planStepsEl.textContent = planStepsText(provenance);
      rawDebugEl.textContent = rawAnswer;
      renderWhySummary(provenance);
      updateFollowupContext(provenance, finalAnswer);
      await renderChart(provenance);
      setStatus("History loaded", "success");
    });
    historyDetailEl.querySelector('[data-history-action="copy-answer"]')?.addEventListener("click", async () => {
      await navigator.clipboard.writeText(finalAnswer);
      setStatus("Answer copied", "success");
    });
  }

  function renderHistory(entries) {
    historyListEl.replaceChildren();
    if (!Array.isArray(entries) || !entries.length) {
      historyListEl.innerHTML = `
        <div class="chart-empty" style="padding: var(--space-8) var(--space-4);">
          <div class="chart-empty-icon">🕐</div>
          <div class="chart-empty-title">No history entries found</div>
        </div>
      `;
      historyDetailEl.innerHTML = `
        <div class="chart-empty" style="padding: var(--space-8) var(--space-4);">
          <div class="chart-empty-icon">📋</div>
          <div class="chart-empty-title">No run selected</div>
          <div class="chart-empty-desc">Run a query or select a history item to inspect it.</div>
        </div>
      `;
      return;
    }
    entries.slice(0, 50).forEach((entry, index) => {
      const item = document.createElement("button");
      item.type = "button";
      item.className = "history-item";
      item.innerHTML = `
        <div class="history-question">${escapeHtml(entry.question || "(no question)")}</div>
        <div class="history-meta">
          <span class="history-meta-dot ${entry.success ? "" : "error"}"></span>
          ${escapeHtml(entry.success ? "Success" : "Failed")} · ${escapeHtml(
            Number.isFinite(entry.execution_ms) ? `${entry.execution_ms}ms` : "0ms",
          )} · ${escapeHtml(formatTimestamp(entry.timestamp))}
        </div>
      `;
      item.addEventListener("click", () => {
        document.querySelectorAll(".history-item.active").forEach((node) => node.classList.remove("active"));
        item.classList.add("active");
        if (entry.question) questionEl.value = entry.question;
        renderHistoryDetail(entry);
      });
      historyListEl.appendChild(item);
      if (index === 0) {
        item.classList.add("active");
        renderHistoryDetail(entry);
      }
    });
  }

  async function fetchHistoryEntries() {
    let response = await fetch("/history");
    if (response.status === 404) {
      response = await fetch("/history.json");
    }
    if (!response.ok) {
      if (response.status === 403) {
        throw new Error("History is unavailable.");
      }
      throw new Error(`HTTP ${response.status}`);
    }
    const payload = await response.json();
    return Array.isArray(payload) ? payload : payload.entries || [];
  }

  async function loadHistory() {
    historyListEl.innerHTML = `
      <div class="chart-empty" style="padding: var(--space-8) var(--space-4);">
        <div class="spinner" style="margin: 0 auto var(--space-3);"></div>
        <div class="chart-empty-title">Loading history...</div>
      </div>
    `;
    try {
      renderHistory(await fetchHistoryEntries());
    } catch (err) {
      historyListEl.innerHTML = `
        <div class="chart-empty" style="padding: var(--space-8) var(--space-4);">
          <div class="chart-empty-icon">⚠️</div>
          <div class="chart-empty-title">History unavailable</div>
          <div class="chart-empty-desc">${escapeHtml(err.message)}</div>
        </div>
      `;
    }
  }

  async function searchHistory() {
    const query = historySearchEl.value.trim();
    if (!query) {
      await loadHistory();
      return;
    }
    historyListEl.innerHTML = `
      <div class="chart-empty" style="padding: var(--space-8) var(--space-4);">
        <div class="spinner" style="margin: 0 auto var(--space-3);"></div>
        <div class="chart-empty-title">Searching history...</div>
      </div>
    `;
    try {
      const response = await fetch("/history/search", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ query }),
      });
      if (!response.ok) {
        if (response.status === 403) throw new Error("History search is unavailable.");
        throw new Error(`HTTP ${response.status}`);
      }
      const payload = await response.json();
      renderHistory(Array.isArray(payload) ? payload : payload.entries || []);
    } catch (err) {
      historyListEl.innerHTML = `
        <div class="chart-empty" style="padding: var(--space-8) var(--space-4);">
          <div class="chart-empty-icon">⚠️</div>
          <div class="chart-empty-title">Search failed</div>
          <div class="chart-empty-desc">${escapeHtml(err.message)}</div>
        </div>
      `;
    }
  }

  async function runDemo() {
    const prompt = questionEl.value.trim();
    if (!prompt) return;

    setRunning(true);
    metricSelect.value = "";
    intervalSelect.value = "all";
    chartModeSelect.value = "auto";
    tablePage = 0;
    answerEl.className = "answer-box";
    answerEl.style.color = "";
    answerEl.textContent = isAdmin
      ? "Running query and waiting for debug provenance..."
      : "Running query...";
    chartEl.innerHTML = `
      <div class="chart-empty">
        <div class="spinner" style="margin: 0 auto var(--space-3);"></div>
        <div class="chart-empty-title">${isAdmin ? "Waiting for row-aligned evidence" : "Running query"}</div>
        <div class="chart-empty-desc">${
          isAdmin
            ? "The admin view will render charts from debug provenance when possible."
            : "The public view will show the grounded answer. Charts appear for runs that include structured evidence."
        }</div>
      </div>
    `;
    chartTypeEl.textContent = "-";
    queriesEl.textContent = "Waiting...";
    provenanceEl.textContent = "Waiting...";
    planStepsEl.textContent = "Waiting...";
    rawDebugEl.textContent = "Waiting...";
    resetWhySummary();

    try {
      let streamedContent = "";
      const response = await fetch("/v1/chat/completions/stream", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          model: "",
          stream: true,
          execute: true,
          dry_run: isAdmin,
          messages: [{ role: "user", content: prompt }],
        }),
      });
      await readSseResponse(response, (event) => {
        const payload = event.payload || {};
        if (event.event === "stage") {
          const stage = payload.stage ? String(payload.stage).replace(/_/g, " ") : "Running";
          const status = payload.status ? String(payload.status) : "";
          setStatus(status ? `${stage}: ${status}` : stage);
        } else if (event.event === "answer") {
          streamedContent += payload.content || "";
          answerEl.textContent = finalAnswerOrStreamingText(streamedContent);
          rawDebugEl.textContent = streamedContent || "Streaming response...";
        } else if (event.event === "error") {
          throw new Error(payload.message || "Streaming request failed.");
        } else if (event.event === "done") {
          setStatus("Done", "success");
        }
      });

      const content = streamedContent;
      lastRawDebug = content;
      rawDebugEl.textContent = content || "No full debug output was returned.";
      const provenance = extractNamedJsonBlock(content, "Provenance");
      const finalAnswer = extractFinalAnswer(content);
      if (!provenance) {
        lastFinalAnswer = finalAnswer || content || "";
        answerEl.textContent = finalAnswer || content || "No final answer was returned.";
        chartTypeEl.textContent = "None";
        rowCountEl.textContent = "-";
        chartSourceEl.textContent = isAdmin ? "Debug response" : "Normal response";
        showEmpty(
          isAdmin
            ? "No chartable evidence was returned in this debug response."
            : "No chartable structured evidence was returned with this normal response. Log in as admin to render charts from debug provenance, or open a previous history run that contains evidence."
        );
        if (isAdmin) {
          provenanceEl.textContent = "No Provenance JSON block was returned. Ensure debug output is enabled by this demo request.";
          queriesEl.textContent = "No executed queries available.";
          planStepsEl.textContent = "No plan steps available.";
        }
        setStatus("Done", "success");
        loadHistory().catch(() => {});
        return;
      }
      lastFinalAnswer = finalAnswer;
      answerEl.style.color = "";
      answerEl.textContent = conciseAnswerText(finalAnswer, provenance);
      provenanceEl.textContent = JSON.stringify(provenance, null, 2);
      queriesEl.textContent = executedQueriesText(provenance);
      planStepsEl.textContent = planStepsText(provenance);
      renderWhySummary(provenance);
      lastProvenance = provenance;
      updateFollowupContext(provenance, finalAnswer);
      await renderChart(provenance);
      setStatus("Done", "success");
      loadHistory().catch(() => {});
    } catch (err) {
      answerEl.textContent = `Demo request failed: ${err.message}`;
      answerEl.className = "answer-box";
      answerEl.style.color = "var(--danger)";
      showEmpty(`Unable to render a chart because the demo request failed: ${err.message}`, true);
      chartTypeEl.textContent = "Error";
      rowCountEl.textContent = "-";
      chartSourceEl.textContent = "Provenance only";
      queriesEl.textContent = "No executed queries available.";
      provenanceEl.textContent = String(err.stack || err.message || err);
      planStepsEl.textContent = "No plan steps available.";
      rawDebugEl.textContent = String(err.stack || err.message || err);
      lastRawDebug = rawDebugEl.textContent;
      setStatus("Error", "error");
    } finally {
      setRunning(false);
    }
  }

  document.querySelectorAll("[data-q]").forEach((button) => {
    button.addEventListener("click", () => {
      questionEl.value = button.dataset.q || "";
      questionEl.focus();
    });
  });

  document.querySelectorAll("[data-prompt-group-target]").forEach((button) => {
    button.addEventListener("click", () => {
      const target = button.dataset.promptGroupTarget;
      document.querySelectorAll("[data-prompt-group-target]").forEach((node) => {
        node.classList.toggle("active", node === button);
      });
      document.querySelectorAll("[data-prompt-group]").forEach((group) => {
        group.classList.toggle("active", group.dataset.promptGroup === target);
      });
    });
  });

  runBtn.addEventListener("click", runDemo);
  clearBtn.addEventListener("click", resetOutputs);
  loginForm?.addEventListener("submit", login);
  exportCsvBtn?.addEventListener("click", () => exportCurrent("csv"));
  exportJsonBtn?.addEventListener("click", () => exportCurrent("json"));
  exportMarkdownBtn?.addEventListener("click", () => exportCurrent("markdown"));
  applyContextBtn?.addEventListener("click", applyFollowupContext);
  clearContextBtn?.addEventListener("click", () => {
    followupContext = null;
    renderFollowupContext();
    setStatus("Context cleared", "success");
  });
  metricSelect.addEventListener("change", () => {
    tablePage = 0;
    rerenderCurrentChart().catch(() => {});
  });
  intervalSelect.addEventListener("change", () => {
    tablePage = 0;
    rerenderCurrentChart().catch(() => {});
  });
  chartModeSelect.addEventListener("change", () => {
    tablePage = 0;
    rerenderCurrentChart().catch(() => {});
  });

  function doReset() {
    metricSelect.value = "";
    intervalSelect.value = "all";
    chartModeSelect.value = "auto";
    tablePage = 0;
    rerenderCurrentChart().catch(() => {});
  }

  resetChartBtn.addEventListener("click", doReset);
  resetChartBtn2?.addEventListener("click", doReset);

  async function copyFullDebugOutput(button) {
    const text = lastRawDebug || rawDebugEl.textContent || "";
    if (!text.trim()) return;
    try {
      await navigator.clipboard.writeText(text);
      const original = button.innerHTML;
      button.innerHTML = '<span>✓</span> Copied!';
      setTimeout(() => {
        button.innerHTML = original;
      }, 1500);
      setStatus("Debug copied", "success");
    } catch (_err) {
      setStatus("Copy failed", "error");
    }
  }

  copyDebugBtn.addEventListener("click", () => {
    copyFullDebugOutput(copyDebugBtn);
  });
  copyFullDebugBtn?.addEventListener("click", () => {
    copyFullDebugOutput(copyFullDebugBtn);
  });

  logoutBtn.addEventListener("click", logout);
  refreshHistoryBtn.addEventListener("click", loadHistory);
  historySearchBtn.addEventListener("click", searchHistory);
  historySearchEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter") searchHistory();
  });

  window.addEventListener("resize", () => {
    if (!lastProvenance) return;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      rerenderCurrentChart().catch(() => {});
    }, 120);
  });

  resetWhySummary();
  renderFollowupContext();
  checkSession().catch(() => {});
})();
