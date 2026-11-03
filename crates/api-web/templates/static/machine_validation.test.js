// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
const {test} = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const source = fs.readFileSync(`${__dirname}/machine_validation.js`, "utf8");

function viewer(responses) {
    const text = () => ({textContent: "", set innerHTML(value) { throw new Error("Logs must not be interpreted as HTML"); }});
    const elements = {
        "validation-logs": {dataset: {url: "/logs.json"}},
        "validation-log-status": text(),
        "validation-stdout": text(),
        "validation-stderr": text(),
    };
    const timers = [];
    const calls = [];
    vm.runInNewContext(source, {
        document: {getElementById: id => elements[id], querySelectorAll: () => []},
        fetch: async (url, options) => {
            calls.push({url, options});
            assert.ok(responses.length, "unexpected request after completion");
            const response = responses.shift();
            return {ok: true, json: async () => response, ...response};
        },
        setTimeout: (callback, delay) => timers.push({callback, delay}),
    });
    return {elements, timers, calls};
}
const settle = () => new Promise(resolve => setImmediate(resolve));

test("keeps stdout/stderr separate, advances the cursor, and stops after final output", async () => {
    const v = viewer([
        {enabled: true, active: true, has_more: true, chunks: [
            {sequence: 1, stream: "stdout", content: "<script>first</script>"},
            {sequence: 2, stream: "stderr", content: "diagnostic"},
        ]},
        {enabled: true, active: false, has_more: false, chunks: [
            {sequence: 3, stream: "stdout", content: "final"},
        ]},
    ]);
    await settle();
    assert.equal(v.timers.length, 1);
    assert.equal(v.timers[0].delay, 0, "drain additional pages immediately");
    await v.timers.shift().callback();
    assert.equal(v.elements["validation-stdout"].textContent, "<script>first</script>final");
    assert.equal(v.elements["validation-stderr"].textContent, "diagnostic");
    assert.deepEqual(v.calls.map(call => call.url), ["/logs.json?after_sequence=0", "/logs.json?after_sequence=2"]);
    assert.ok(v.calls.every(call => call.options.redirect === "error" && call.options.cache === "no-store"));
    assert.equal(v.timers.length, 0);
});

test("reports expired authentication and disabled storage without polling forever", async () => {
    for (const [response, expected] of [
        [{ok: false, status: 401}, /401/],
        [{enabled: false, active: false, has_more: false, chunks: []}, /disabled/],
    ]) {
        const v = viewer([response]);
        await settle();
        assert.match(v.elements["validation-log-status"].textContent, expected);
        assert.equal(v.timers.length, 0);
    }
});

test("refreshes live progress and reloads terminal results once on completion", async () => {
    const timers = [];
    let active = true;
    let reloads = 0;
    const live = {
        dataset: {url: "/run/live"},
        querySelector: () => active,
        set innerHTML(html) { active = html.includes('data-active="true"'); },
    };
    const status = {textContent: ""};
    vm.runInNewContext(source, {
        document: {
            getElementById: id => ({"validation-live": live, "validation-refresh-status": status})[id],
            querySelectorAll: () => [],
        },
        fetch: async () => ({ok: true, text: async () => '<section data-active="false"></section>'}),
        setTimeout: callback => timers.push(callback),
        location: {reload: () => reloads++},
    });
    assert.equal(timers.length, 1);
    await timers.shift()();
    assert.equal(active, false);
    assert.equal(reloads, 1);
    assert.equal(timers.length, 0);
});

test("updates an active list row without replacing the surrounding machine page", async () => {
    const timers = [];
    const progress = {dataset: {field: "progress"}, textContent: "0/1"};
    const row = {
        dataset: {validationUrl: "/run/live"},
        querySelectorAll: () => [progress],
    };
    class Parser {
        parseFromString() {
            return {querySelector: selector => selector.includes('data-field="progress"') ? {textContent: "1/1"} : null};
        }
    }
    vm.runInNewContext(source, {
        document: {getElementById: () => null, querySelectorAll: () => [row]},
        DOMParser: Parser,
        fetch: async () => ({ok: true, text: async () => "terminal snapshot"}),
        setTimeout: callback => timers.push(callback),
    });
    assert.equal(timers.length, 1);
    await timers.shift()();
    assert.equal(progress.textContent, "1/1");
    assert.equal(timers.length, 0);
});
