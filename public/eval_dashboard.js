(function () {
  const bundledSnapshot = {
    source: "eval/results/golden_debug.json",
    generatedAt: "2026-05-05 15:12",
    cases: [
      { id: "db_first_two_wind_farms", question: "List the first 2 offshore wind farms with name and shortName.", status: "pass", failure_category: "pass", matched: 4, total: 4, recall: 1, latency_ms: 5605, evidence_row_count: 2, provider_total_tokens: 9030, final_answer: "Found 2 result(s): name: Wind Farm 1, shortName: WF1 | name: Wind Farm 2, shortName: WF2." },
      { id: "db_wind_farm_1_details", question: "Show details for wind farm Wind Farm 1.", status: "partial", failure_category: "answer_omitted_expected_details", matched: 1, total: 5, recall: 0.2, latency_ms: 548, grounding_overall: "needs_confirmation", final_answer: "I found likely entity candidates, but not enough grounding to choose confidently." },
      { id: "db_wind_farm_wf3_details", question: "Show details for wind farm with shortName WF3.", status: "pass", failure_category: "pass", matched: 5, total: 5, recall: 1, latency_ms: 13211, evidence_row_count: 1, provider_total_tokens: 10260, final_answer: "Found 1 result(s): name: Wind Farm 3, shortName: WF3, commercialDateTimeOfOperation: 2022-05-29T10:00:38.526Z." },
      { id: "db_top_three_farms_by_rated_capacity", question: "Top 3 wind farms by ratedCapacity.", status: "pass", failure_category: "pass", matched: 6, total: 6, recall: 1, latency_ms: 7109, evidence_row_count: 3, provider_total_tokens: 8247, final_answer: "Found 3 result(s): Wind Farm 4, Wind Farm 2, and Wind Farm 5 by ratedCapacity." },
      { id: "db_total_turbines", question: "Show the total number of turbines.", status: "pass", failure_category: "pass", matched: 1, total: 1, recall: 1, latency_ms: 9995, evidence_row_count: 1, provider_total_tokens: 9775, final_answer: "The total number of turbines is 120." },
      { id: "db_turbine_count_per_farm", question: "Count turbines per wind farm.", status: "fail", failure_category: "empty_backend_relation", matched: 0, total: 7, recall: 0, latency_ms: 11577, evidence_row_count: 1, provider_total_tokens: 9453, final_answer: "Backend returned no child rows for queryOffshoreWindTurbine.partOfOffshoreWindFarm." },
      { id: "db_highest_avg_downtime_farm", question: "Which wind farm has the highest average accumulatedDowntime?", status: "fail", failure_category: "empty_backend_relation", matched: 0, total: 2, recall: 0, latency_ms: 20031, evidence_row_count: 1, provider_total_tokens: 9278, final_answer: "Backend returned no child rows for queryOffshoreWindTurbine.partOfOffshoreWindFarm." },
      { id: "db_first_five_turbines_in_wind_farm_1", question: "List turbines in wind farm Wind Farm 1.", status: "fail", failure_category: "answer_mismatch_or_wrong_plan", matched: 0, total: 10, recall: 0, latency_ms: 518, grounding_overall: "needs_confirmation", final_answer: "Stopped for entity confirmation before execution." },
      { id: "db_turbine_t3_details", question: "Show turbine T3 details.", status: "pass", failure_category: "pass", matched: 4, total: 4, recall: 1, latency_ms: 7812, evidence_row_count: 1, provider_total_tokens: 9898, final_answer: "Found 1 result(s): name: Turbine 3, shortName: T3." },
      { id: "db_turbine_t120_details", question: "Show turbine T120 details.", status: "pass", failure_category: "pass", matched: 4, total: 4, recall: 1, latency_ms: 9663, evidence_row_count: 1, provider_total_tokens: 9773, final_answer: "Found 1 result(s): name: Turbine 120, shortName: T120." },
      { id: "db_compare_turbine_115_109", question: "Compare average accumulatedDowntime between \"turbine 115\" and \"turbine 109\".", status: "partial", failure_category: "answer_omitted_expected_details", matched: 2, total: 5, recall: 0.4, latency_ms: 636, grounding_overall: "needs_confirmation", final_answer: "Stopped for entity confirmation before execution." },
      { id: "db_highest_downtime_turbine", question: "Which turbine has the highest accumulatedDowntime?", status: "pass", failure_category: "pass", matched: 3, total: 3, recall: 1, latency_ms: 8177, evidence_row_count: 1, provider_total_tokens: 8421, schema_retrieval_confidence: "low", final_answer: "Found 1 result(s): name: Turbine 114, shortName: T114, accumulatedDowntime: 499.8353723115927." },
      { id: "db_top_three_turbines_by_downtime", question: "Top 3 turbines by accumulatedDowntime.", status: "pass", failure_category: "pass", matched: 9, total: 9, recall: 1, latency_ms: 6560, evidence_row_count: 3, provider_total_tokens: 8281, final_answer: "Found 3 result(s): Turbine 114, Turbine 43, and Turbine 56." },
      { id: "db_list_offshore_substations", question: "List offshore substations with name and shortName.", status: "pass", failure_category: "pass", matched: 12, total: 12, recall: 1, latency_ms: 8627, evidence_row_count: 6, provider_total_tokens: 8660, final_answer: "Found 6 offshore substations." },
      { id: "db_list_onshore_substations", question: "List onshore substations with name and shortName.", status: "pass", failure_category: "pass", matched: 12, total: 12, recall: 1, latency_ms: 4971, evidence_row_count: 6, provider_total_tokens: 8714, schema_retrieval_confidence: "low", final_answer: "Found 6 onshore substations." },
      { id: "db_offshore_substation_oss3", question: "Show offshore substation OSS3 details.", status: "pass", failure_category: "pass", matched: 2, total: 2, recall: 1, latency_ms: 9740, evidence_row_count: 1, provider_total_tokens: 9999, final_answer: "OSS3 is Offshore Substation 3." },
      { id: "db_onshore_substation_ons2", question: "Show onshore substation ONS2 details.", status: "pass", failure_category: "pass", matched: 2, total: 2, recall: 1, latency_ms: 6304, evidence_row_count: 1, provider_total_tokens: 9640, final_answer: "ONS2 is Onshore Substation 2." },
      { id: "db_tag_counts_plant_2", question: "Count tags by categoryDescription for plantId PLANT-  2.", status: "pass", failure_category: "pass", matched: 9, total: 9, recall: 1, latency_ms: 16620, evidence_row_count: 5, provider_total_tokens: 10686, final_answer: "Tag counts by categoryDescription for PLANT- 2." },
      { id: "db_top_tag_categories_plant_4", question: "Top 5 tag categories by count for plantId PLANT-  4.", status: "pass", failure_category: "pass", matched: 11, total: 11, recall: 1, latency_ms: 11525, evidence_row_count: 5, provider_total_tokens: 10018, final_answer: "Top tag categories for PLANT- 4." },
      { id: "db_compare_weather_vs_electrical_plant_5", question: "Compare tag counts between categoryDescription Weather and Electrical for plantId PLANT-  5.", status: "pass", failure_category: "pass", matched: 6, total: 6, recall: 1, latency_ms: 17109, evidence_row_count: 2, provider_total_tokens: 11967, final_answer: "Weather has 300 tags and Electrical has 200." },
      { id: "db_bottom_two_farms_by_rated_capacity", question: "Bottom 2 wind farms by ratedCapacity.", status: "pass", failure_category: "pass", matched: 6, total: 6, recall: 1, latency_ms: 6735, evidence_row_count: 2, provider_total_tokens: 8379, final_answer: "Bottom two wind farms are Wind Farm 3 and Wind Farm 1." },
      { id: "db_highest_wind_downtime_turbine", question: "Which turbine has the highest accumulatedWindDowntime?", status: "pass", failure_category: "pass", matched: 3, total: 3, recall: 1, latency_ms: 11813, evidence_row_count: 1, provider_total_tokens: 8393, schema_retrieval_confidence: "low", final_answer: "Turbine 53 has the highest accumulatedWindDowntime." },
      { id: "db_turbine_count_by_string_name", question: "Count turbines by stringName.", status: "pass", failure_category: "pass", matched: 13, total: 13, recall: 1, latency_ms: 8414, evidence_row_count: 12, provider_total_tokens: 10295, final_answer: "Each stringName has 10 turbines." },
      { id: "db_tag_status_counts_plant_1", question: "Count tags by status for plantId PLANT-  1.", status: "partial", failure_category: "answer_omitted_expected_details", matched: 4, total: 5, recall: 0.8, latency_ms: 9183, evidence_row_count: 2, provider_total_tokens: 10679, final_answer: "ACTIVE 1,807 and INACTIVE 193." },
      { id: "db_tag_system_counts_plant_3", question: "Count tags by system for plantId PLANT-  3.", status: "pass", failure_category: "pass", matched: 8, total: 8, recall: 1, latency_ms: 12094, evidence_row_count: 4, provider_total_tokens: 10567, final_answer: "Metocean 600, PowerTrain 500, SCADA 500, Electrical 400." },
    ],
  };

  const summaryCards = document.getElementById("summaryCards");
  const breakdownBars = document.getElementById("breakdownBars");
  const priorityList = document.getElementById("priorityList");
  const caseList = document.getElementById("caseList");
  const caseFilter = document.getElementById("caseFilter");
  const caseSort = document.getElementById("caseSort");
  const evalFile = document.getElementById("evalFile");
  const loadBundledBtn = document.getElementById("loadBundledBtn");
  const tryFetchBtn = document.getElementById("tryFetchBtn");
  let currentCases = [];
  let currentSource = "";

  function normalizeRun(payload, source) {
    const cases = Array.isArray(payload) ? payload : payload?.cases || payload?.results || [];
    return {
      source: payload?.source || source || "Loaded JSON",
      generatedAt: payload?.generatedAt || payload?.generated_at || "",
      cases: cases.filter((row) => row && typeof row === "object"),
    };
  }

  function countBy(rows, key) {
    return rows.reduce((acc, row) => {
      const value = String(row[key] || "(none)");
      acc[value] = (acc[value] || 0) + 1;
      return acc;
    }, {});
  }

  function average(values) {
    const nums = values.filter((value) => Number.isFinite(value));
    if (!nums.length) return 0;
    return nums.reduce((sum, value) => sum + value, 0) / nums.length;
  }

  function fmtNumber(value) {
    return Math.round(value).toLocaleString();
  }

  function fmtPercent(value) {
    return `${(value * 100).toFixed(1)}%`;
  }

  function escapeHtml(value) {
    return String(value ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function chatUrl(question) {
    const params = new URLSearchParams({
      q: question || "",
      debug: "1",
      execute: "1",
    });
    return `/?${params.toString()}`;
  }

  function summary(rows) {
    const total = rows.length;
    const pass = rows.filter((row) => row.status === "pass").length;
    const partial = rows.filter((row) => row.status === "partial").length;
    const fail = rows.filter((row) => row.status === "fail" || row.status === "error").length;
    const recall = average(rows.map((row) => Number(row.recall)).filter(Number.isFinite));
    const latency = average(rows.map((row) => Number(row.latency_ms)).filter(Number.isFinite));
    const tokens = rows.reduce((sum, row) => sum + (Number(row.provider_total_tokens) || 0), 0);
    return { total, pass, partial, fail, recall, latency, tokens };
  }

  function renderCards(rows) {
    const s = summary(rows);
    const passRate = s.total ? s.pass / s.total : 0;
    const cards = [
      ["Pass Rate", fmtPercent(passRate), `${s.pass}/${s.total} exact passes`],
      ["Average Recall", fmtPercent(s.recall), "must-include checks matched"],
      ["Average Latency", `${(s.latency / 1000).toFixed(2)}s`, "mean end-to-end time"],
      ["Provider Tokens", fmtNumber(s.tokens), "reported total tokens"],
    ];
    summaryCards.innerHTML = cards
      .map(
        ([label, value, note]) => `
          <article class="card">
            <div class="card-label">${escapeHtml(label)}</div>
            <div class="card-value">${escapeHtml(value)}</div>
            <div class="card-note">${escapeHtml(note)}</div>
          </article>
        `,
      )
      .join("");
  }

  function renderBreakdown(rows) {
    const statusCounts = countBy(rows, "status");
    const categoryCounts = countBy(rows, "failure_category");
    const entries = [
      ...Object.entries(statusCounts).map(([label, value]) => ({ label, value, className: label })),
      ...Object.entries(categoryCounts)
        .filter(([label]) => label !== "pass")
        .map(([label, value]) => ({ label, value, className: "info" })),
    ];
    const max = Math.max(1, ...entries.map((entry) => entry.value));
    breakdownBars.innerHTML = entries
      .map(
        (entry) => `
          <div class="bar-row">
            <div>${escapeHtml(entry.label)}</div>
            <div class="bar-track">
              <div class="bar-fill ${escapeHtml(entry.className)}" style="width: ${(entry.value / max) * 100}%"></div>
            </div>
            <div>${entry.value}</div>
          </div>
        `,
      )
      .join("");
  }

  function priorityItems(rows) {
    const confirmation = rows.filter((row) => row.grounding_overall === "needs_confirmation");
    const backend = rows.filter((row) => row.failure_category === "empty_backend_relation");
    const scoring = rows.filter((row) => row.id === "db_tag_status_counts_plant_1");
    const weakRetrieval = rows.filter((row) => row.schema_retrieval_confidence === "low");
    return [
      {
        title: "Reduce false confirmation stops",
        meta: `${confirmation.length} case(s): ${confirmation.map((row) => row.id).join(", ") || "none"}.`,
      },
      {
        title: "Normalize numeric scoring",
        meta: scoring.length
          ? "Comma-formatted values such as 1,807 should match checks such as 1807."
          : "No obvious numeric-format scoring issue in this run.",
      },
      {
        title: "Keep backend relation gaps explicit",
        meta: `${backend.length} case(s) depend on relation data that the backend returned as empty/null.`,
      },
      {
        title: "Watch weak retrieval",
        meta: `${weakRetrieval.length} passing case(s) still had low retrieval confidence; useful calibration targets.`,
      },
    ];
  }

  function renderPriorities(rows) {
    priorityList.innerHTML = priorityItems(rows)
      .map(
        (item) => `
          <article class="priority-item">
            <div class="priority-title">${escapeHtml(item.title)}</div>
            <div class="priority-meta">${escapeHtml(item.meta)}</div>
          </article>
        `,
      )
      .join("");
  }

  function sortedCases(rows) {
    const filter = caseFilter.value;
    let out = rows.slice();
    if (filter === "non-pass") out = out.filter((row) => row.status !== "pass");
    if (filter === "pass") out = out.filter((row) => row.status === "pass");
    if (filter === "partial") out = out.filter((row) => row.status === "partial");
    if (filter === "fail") out = out.filter((row) => row.status === "fail" || row.status === "error");

    const sort = caseSort.value;
    const statusRank = { fail: 0, error: 0, partial: 1, pass: 2 };
    out.sort((a, b) => {
      if (sort === "latency") return (Number(b.latency_ms) || 0) - (Number(a.latency_ms) || 0);
      if (sort === "tokens") return (Number(b.provider_total_tokens) || 0) - (Number(a.provider_total_tokens) || 0);
      if (sort === "id") return String(a.id).localeCompare(String(b.id));
      return (statusRank[a.status] ?? 9) - (statusRank[b.status] ?? 9) || String(a.id).localeCompare(String(b.id));
    });
    return out;
  }

  function renderCases(rows) {
    const visible = sortedCases(rows);
    if (!visible.length) {
      caseList.innerHTML = '<div class="empty">No cases match this filter.</div>';
      return;
    }
    caseList.innerHTML = visible
      .map((row) => {
        const recall = Number.isFinite(Number(row.recall)) ? fmtPercent(Number(row.recall)) : "-";
        const latency = Number.isFinite(Number(row.latency_ms)) ? `${(Number(row.latency_ms) / 1000).toFixed(2)}s` : "-";
        const tokens = Number(row.provider_total_tokens) ? fmtNumber(Number(row.provider_total_tokens)) : "-";
        return `
          <article class="case">
            <div class="case-top">
              <div class="case-id">${escapeHtml(row.id || "case")}</div>
              <span class="status ${escapeHtml(row.status || "unknown")}">${escapeHtml(row.status || "unknown")}</span>
            </div>
            <div class="case-question">${escapeHtml(row.question || "")}</div>
            <div class="case-meta">
              category=${escapeHtml(row.failure_category || "-")} · recall=${escapeHtml(recall)} · latency=${escapeHtml(latency)} · tokens=${escapeHtml(tokens)}
            </div>
            <div class="case-answer">${escapeHtml(row.final_answer || "No final answer captured.")}</div>
            <div class="case-actions">
              <a class="case-action" href="${escapeHtml(chatUrl(row.question || ""))}">Load in Chat UI</a>
            </div>
          </article>
        `;
      })
      .join("");
  }

  function render(data) {
    currentCases = data.cases;
    currentSource = data.source;
    document.title = `Zephyr Eval Dashboard - ${currentCases.length} cases`;
    renderCards(currentCases);
    renderBreakdown(currentCases);
    renderPriorities(currentCases);
    renderCases(currentCases);
  }

  async function loadFile(file) {
    const text = await file.text();
    render(normalizeRun(JSON.parse(text), file.name));
  }

  async function tryFetchLatest() {
    const candidates = [
      "/eval/results/golden_debug.json",
      "/results/golden_debug.json",
      "/golden_debug.json",
    ];
    for (const path of candidates) {
      try {
        const response = await fetch(path, { cache: "no-store" });
        if (!response.ok) continue;
        const payload = await response.json();
        render(normalizeRun(payload, path));
        return;
      } catch (_err) {
        // Try the next likely path; the bundled snapshot remains available.
      }
    }
    alert(`Could not fetch a latest eval JSON from the static server. Showing bundled snapshot: ${currentSource}`);
  }

  evalFile.addEventListener("change", () => {
    const file = evalFile.files?.[0];
    if (file) loadFile(file).catch((err) => alert(`Could not load eval JSON: ${err.message}`));
  });
  loadBundledBtn.addEventListener("click", () => render(normalizeRun(bundledSnapshot, bundledSnapshot.source)));
  tryFetchBtn.addEventListener("click", () => tryFetchLatest());
  caseFilter.addEventListener("change", () => renderCases(currentCases));
  caseSort.addEventListener("change", () => renderCases(currentCases));

  render(normalizeRun(bundledSnapshot, bundledSnapshot.source));
})();
