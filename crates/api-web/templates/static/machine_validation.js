// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
"use strict";

// Stop if authentication expires: following an OAuth redirect would otherwise
// replace validation content with the sign-in page.
async function validationFetch(url) {
    const response = await fetch(url, {cache: "no-store", redirect: "error"});
    if (!response.ok) throw new Error(`Request failed (${response.status}). Refresh to sign in or retry.`);
    return response;
}

const live = document.getElementById("validation-live");
if (live && live.querySelector('[data-active="true"]')) {
    const refresh = async () => {
        try {
            const html = await (await validationFetch(live.dataset.url)).text();
            live.innerHTML = html;
            document.getElementById("validation-refresh-status").textContent = "Progress updated.";
            if (live.querySelector('[data-active="true"]')) setTimeout(refresh, 2000);
            else location.reload(); // Reload terminal results once the run finishes.
        } catch (error) {
            document.getElementById("validation-refresh-status").textContent = error.message;
        }
    };
    setTimeout(refresh, 2000);
}

const logs = document.getElementById("validation-logs");
if (logs) {
    let sequence = 0;
    const status = document.getElementById("validation-log-status");
    const limit = 1024 * 1024;
    const poll = async () => {
        try {
            const page = await (await validationFetch(`${logs.dataset.url}?after_sequence=${sequence}`)).json();
            if (!page.enabled) {
                status.textContent = "Attempt-log storage is disabled for this site.";
                return;
            }
            for (const chunk of page.chunks) {
                if (chunk.sequence <= sequence) throw new Error("Log cursor did not advance. Refresh to retry.");
                sequence = chunk.sequence;
                const output = document.getElementById(`validation-${chunk.stream}`);
                if (!output) throw new Error("Unknown log stream.");
                // Treat plugin output as text, never as markup. Bound browser
                // memory independently of the server's configured storage limit.
                let tail = (output.textContent + chunk.content).slice(-limit);
                // Avoid cutting a UTF-16 surrogate pair at the retained boundary.
                if (tail.charCodeAt(0) >= 0xDC00 && tail.charCodeAt(0) <= 0xDFFF) tail = tail.slice(1);
                output.textContent = tail;
            }
            if (page.has_more && page.chunks.length === 0) throw new Error("Empty log page with more data. Refresh to retry.");
            status.textContent = page.active ? "Following stored logs; older output may be omitted from this view." : "Attempt finished; showing stored logs. Older output may be omitted from this view.";
            if (page.has_more || page.active) setTimeout(poll, page.has_more ? 0 : 2000);
        } catch (error) {
            status.textContent = error.message;
        }
    };
    poll();
}

for (const row of document.querySelectorAll('[data-validation-active="true"]')) {
    const refreshRow = async () => {
        try {
            const html = await (await validationFetch(row.dataset.validationUrl)).text();
            const snapshot = new DOMParser().parseFromString(html, "text/html");
            for (const cell of row.querySelectorAll("[data-field]")) {
                const value = snapshot.querySelector(`[data-field="${cell.dataset.field}"]`);
                if (value) cell.textContent = value.textContent;
            }
            if (snapshot.querySelector('[data-active="true"]')) setTimeout(refreshRow, 5000);
        } catch (error) {
            // Preserve the last known values, and make a stale row explicit.
            row.title = error.message;
            const progress = row.querySelector('[data-field="progress"]');
            if (progress) progress.textContent = "Refresh failed";
        }
    };
    setTimeout(refreshRow, 5000);
}
