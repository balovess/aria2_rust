#!/usr/bin/env node

// Throwaway offline simulator for choosing HTTP/1.1 and HTTP/2 default limits.
// It is a synthetic model, not a measurement of a real origin. The purpose is
// to compare relative policy risk across explicit server-capacity regimes.

import assert from "node:assert/strict";

const RANGE_COUNT = Number(process.env.SIM_RANGES ?? 256);
const RUNS = Number(process.env.SIM_RUNS ?? 40);
const TOP = Number(process.env.SIM_TOP ?? 15);
const ONLY = new Set(
    (process.env.SIM_ONLY ?? "")
        .split(",")
        .map((name) => name.trim())
        .filter(Boolean),
);
const MAX_SECONDS = 24 * 60 * 60;
const MAX_EVENTS = 200_000;

assert.ok(Number.isInteger(RANGE_COUNT) && RANGE_COUNT > 0);
assert.ok(Number.isInteger(RUNS) && RUNS > 0);
assert.ok(Number.isInteger(TOP) && TOP > 0);

function randomFrom(seed) {
    let state = seed >>> 0;
    return () => {
        state = (state + 0x6d2b79f5) >>> 0;
        let value = state;
        value = Math.imul(value ^ (value >>> 15), value | 1);
        value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
        return ((value ^ (value >>> 14)) >>> 0) / 0x1_0000_0000;
    };
}

const ADAPTIVE_PRESETS = {
    off: {
        name: "off",
        enabled: false,
        reductionPercent: 25,
        recoveryInterval: 10,
        recoveryMin: 8,
        recoveryMax: 32,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 0,
        startIntervalMaxMs: 0,
        rateRecoverySuccesses: 64,
        rateMinIntervalMs: 10,
        backoffResetAfter: 60,
    },
    current: {
        name: "current",
        enabled: true,
        reductionPercent: 25,
        recoveryInterval: 10,
        recoveryMin: 8,
        recoveryMax: 32,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 1000,
        startIntervalMaxMs: 4000,
        rateRecoverySuccesses: 64,
        rateMinIntervalMs: 10,
        backoffResetAfter: 60,
    },
    gentle: {
        name: "gentle",
        enabled: true,
        reductionPercent: 15,
        recoveryInterval: 10,
        recoveryMin: 8,
        recoveryMax: 32,
        backoffBase: 10,
        backoffMax: 120,
        startIntervalMs: 250,
        startIntervalMaxMs: 2000,
        rateRecoverySuccesses: 32,
        rateMinIntervalMs: 20,
        backoffResetAfter: 60,
    },
    strong: {
        name: "strong",
        enabled: true,
        reductionPercent: 35,
        recoveryInterval: 20,
        recoveryMin: 16,
        recoveryMax: 48,
        backoffBase: 30,
        backoffMax: 300,
        startIntervalMs: 1000,
        startIntervalMaxMs: 4000,
        rateRecoverySuccesses: 64,
        rateMinIntervalMs: 50,
        backoffResetAfter: 60,
    },
    fastProbe: {
        name: "fast-probe",
        enabled: true,
        reductionPercent: 25,
        recoveryInterval: 5,
        recoveryMin: 8,
        recoveryMax: 16,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 500,
        startIntervalMaxMs: 2000,
        rateRecoverySuccesses: 32,
        rateMinIntervalMs: 10,
        backoffResetAfter: 60,
    },
    slowProbe: {
        name: "slow-probe",
        enabled: true,
        reductionPercent: 25,
        recoveryInterval: 20,
        recoveryMin: 32,
        recoveryMax: 64,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 1000,
        startIntervalMaxMs: 4000,
        rateRecoverySuccesses: 128,
        rateMinIntervalMs: 50,
        backoffResetAfter: 60,
    },
    paced: {
        name: "paced",
        enabled: true,
        reductionPercent: 25,
        recoveryInterval: 10,
        recoveryMin: 8,
        recoveryMax: 32,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 2000,
        startIntervalMaxMs: 4000,
        rateRecoverySuccesses: 32,
        rateMinIntervalMs: 50,
        backoffResetAfter: 60,
    },
    halfDrop: {
        name: "half-drop",
        enabled: true,
        reductionPercent: 50,
        recoveryInterval: 10,
        recoveryMin: 8,
        recoveryMax: 32,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 1000,
        startIntervalMaxMs: 4000,
        rateRecoverySuccesses: 64,
        rateMinIntervalMs: 10,
        backoffResetAfter: 60,
    },
    halfFast: {
        name: "half-fast",
        enabled: true,
        reductionPercent: 50,
        recoveryInterval: 5,
        recoveryMin: 8,
        recoveryMax: 16,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 500,
        startIntervalMaxMs: 2000,
        rateRecoverySuccesses: 32,
        rateMinIntervalMs: 10,
        backoffResetAfter: 60,
    },
    halfFastPaced: {
        name: "half-fast-paced",
        enabled: true,
        reductionPercent: 50,
        recoveryInterval: 5,
        recoveryMin: 8,
        recoveryMax: 16,
        backoffBase: 20,
        backoffMax: 300,
        startIntervalMs: 1000,
        startIntervalMaxMs: 4000,
        rateRecoverySuccesses: 64,
        rateMinIntervalMs: 10,
        backoffResetAfter: 60,
    },
};

function environment(name, protocol, overrides) {
    return {
        name,
        protocol,
        serviceMin: 3,
        serviceMax: 12,
        streamCapacity: 64,
        connectionCapacity: 64,
        serverStreamsPerSession: 16,
        tokenRate: Infinity,
        tokenBurst: Infinity,
        burstStarts: Infinity,
        burstWindow: 2,
        banRefusals: Infinity,
        banWindow: 120,
        banDuration: 180,
        stallProbability: 0,
        stallMin: 2,
        stallMax: 12,
        knee: 64,
        penalty: 0.015,
        capacities: null,
        capacityInterval: 45,
        ...overrides,
    };
}

const ENVIRONMENTS = [
    environment("h1-generous", "h1", {
        serviceMin: 2,
        serviceMax: 8,
        streamCapacity: 64,
        connectionCapacity: 64,
    }),
    environment("h1-connection-cap-4", "h1", {
        streamCapacity: 4,
        connectionCapacity: 4,
        banRefusals: 6,
        banWindow: 60,
        banDuration: 120,
    }),
    environment("h1-connection-cap-8", "h1", {
        streamCapacity: 8,
        connectionCapacity: 8,
        serviceMin: 3,
        serviceMax: 12,
        banRefusals: 8,
        banWindow: 120,
        banDuration: 120,
    }),
    environment("h1-token-bucket", "h1", {
        tokenRate: 3,
        tokenBurst: 4,
        banRefusals: 8,
        banWindow: 120,
        banDuration: 180,
    }),
    environment("h1-waf-burst", "h1", {
        burstStarts: 3,
        burstWindow: 2,
        banRefusals: 3,
        banWindow: 180,
        banDuration: 180,
        serviceMin: 1,
        serviceMax: 5,
    }),
    environment("h1-changing-capacity", "h1", {
        capacities: [2, 4, 8, 12],
        capacityInterval: 45,
        banRefusals: 8,
        banWindow: 120,
        banDuration: 60,
    }),
    environment("h1-stalls", "h1", {
        stallProbability: 0.25,
        stallMin: 2,
        stallMax: 12,
        streamCapacity: 10,
        connectionCapacity: 10,
    }),
    environment("h2-generous", "h2", {
        serviceMin: 2,
        serviceMax: 8,
        streamCapacity: 64,
        connectionCapacity: 8,
        serverStreamsPerSession: 16,
    }),
    environment("h2-session-cap-2", "h2", {
        streamCapacity: 16,
        connectionCapacity: 2,
        serverStreamsPerSession: 8,
        banRefusals: 6,
        banWindow: 60,
        banDuration: 120,
    }),
    environment("h2-stream-cap-4", "h2", {
        streamCapacity: 4,
        connectionCapacity: 8,
        serverStreamsPerSession: 4,
        banRefusals: 8,
        banWindow: 120,
        banDuration: 120,
    }),
    environment("h2-token-bucket", "h2", {
        streamCapacity: 32,
        connectionCapacity: 8,
        serverStreamsPerSession: 8,
        tokenRate: 4,
        tokenBurst: 6,
        banRefusals: 8,
        banWindow: 120,
        banDuration: 180,
    }),
    environment("h2-waf-burst", "h2", {
        streamCapacity: 32,
        connectionCapacity: 8,
        serverStreamsPerSession: 8,
        burstStarts: 4,
        burstWindow: 2,
        banRefusals: 4,
        banWindow: 180,
        banDuration: 180,
        serviceMin: 1,
        serviceMax: 5,
    }),
    environment("h2-changing-capacity", "h2", {
        streamCapacity: 16,
        connectionCapacity: 4,
        serverStreamsPerSession: 8,
        capacities: [4, 8, 16, 24],
        capacityInterval: 45,
        banRefusals: 8,
        banWindow: 120,
        banDuration: 60,
    }),
];

function candidates(protocol) {
    const result = [];
    if (protocol === "h1") {
        for (const connections of [1, 2, 4, 6, 8, 10, 12, 16]) {
            for (const [name, preset] of Object.entries(ADAPTIVE_PRESETS)) {
                result.push({
                    name: `h1-x${connections}-${name}`,
                    protocol,
                    split: 16,
                    connections,
                    sessions: 1,
                    streams: 1,
                    adaptive: preset,
                });
            }
        }
    } else {
        for (const sessions of [1, 2, 4, 8]) {
            for (const streams of [1, 2, 4, 8, 16]) {
                if (sessions * streams > 32) {
                    continue;
                }
                for (const [name, preset] of Object.entries(ADAPTIVE_PRESETS)) {
                    result.push({
                        name: `h2-s${sessions}-n${streams}-${name}`,
                        protocol,
                        split: 16,
                        connections: 10,
                        sessions,
                        streams,
                        adaptive: preset,
                    });
                }
            }
        }
    }
    return result;
}

function effectiveCeiling(config) {
    if (config.protocol === "h1") {
        return Math.max(1, Math.min(config.split, config.connections));
    }
    return Math.max(
        1,
        Math.min(config.split, config.sessions * config.streams),
    );
}

function activeSessions(config, active) {
    if (config.protocol === "h1") {
        return active;
    }
    return Math.ceil(active / Math.max(1, config.streams));
}

function run(env, config, seed) {
    const random = randomFrom(seed);
    const settings = config.adaptive;
    const ceiling = effectiveCeiling(config);
    let now = 0;
    let pending = RANGE_COUNT;
    let completed = 0;
    let active = 0;
    let target = ceiling;
    let completions = [];
    let capacityIndex = 0;
    let capacityAt = 0;
    let streamCapacity = env.capacities ? env.capacities[0] : env.streamCapacity;
    let tokens = env.tokenBurst;
    let tokenAt = 0;
    let banUntil = 0;
    let cooldownUntil = 0;
    let nextStartAt = 0;
    let startInterval = 0;
    let capacityFeedback = false;
    let successesSinceAdjustment = 0;
    let successesSinceRate = 0;
    let lastAdjustment = 0;
    let lastCapacityRejection = -Infinity;
    let backoffSteps = 0;
    let refusals = 0;
    let bans = 0;
    let localRetries = 0;
    let startTimes = [];
    let refusalTimes = [];
    let events = 0;

    const refill = (time) => {
        if (!Number.isFinite(env.tokenRate)) {
            return;
        }
        tokens = Math.min(
            env.tokenBurst,
            tokens + Math.max(0, time - tokenAt) * env.tokenRate,
        );
        tokenAt = time;
    };

    const threshold = () =>
        Math.min(settings.recoveryMax, Math.max(settings.recoveryMin, target));

    while (completed < RANGE_COUNT && now < MAX_SECONDS && events++ < MAX_EVENTS) {
        if (env.capacities && now >= capacityAt) {
            capacityIndex = (capacityIndex + 1) % env.capacities.length;
            streamCapacity = env.capacities[capacityIndex];
            capacityAt = now + env.capacityInterval;
        }
        if (now - lastCapacityRejection >= settings.backoffResetAfter) {
            backoffSteps = 0;
        }
        refill(now);

        const blockedUntil = Math.max(
            banUntil,
            settings.enabled ? cooldownUntil : now,
            nextStartAt,
        );
        const canAdmit =
            pending > 0
            && active < target
            && now >= banUntil
            && (!settings.enabled || now >= cooldownUntil)
            && now >= nextStartAt;

        if (canAdmit) {
            const sessions = activeSessions(config, active);
            const overStreams = active >= streamCapacity;
            const overConnections = config.protocol === "h1"
                ? sessions >= env.connectionCapacity
                : sessions + (active % Math.max(1, config.streams) === 0 ? 1 : 0)
                    > env.connectionCapacity;
            const overSessionStreams = config.protocol === "h2"
                && active % Math.max(1, Math.min(config.streams, env.serverStreamsPerSession)) === 0
                && sessions >= env.connectionCapacity;
            startTimes = startTimes.filter((time) => now - time < env.burstWindow);
            refusalTimes = refusalTimes.filter((time) => now - time < env.banWindow);
            const burstLimited = startTimes.length >= env.burstStarts;
            const rateLimited = tokens < 1;
            const rejected = overStreams
                || overConnections
                || overSessionStreams
                || burstLimited
                || rateLimited;

            if (rejected) {
                refusals++;
                refusalTimes.push(now);
                if (refusalTimes.length >= env.banRefusals) {
                    bans++;
                    banUntil = now + env.banDuration;
                    refusalTimes = [];
                }
                if (settings.enabled) {
                    if (lastCapacityRejection > -Infinity) {
                        // One episode is counted per quiet-window-separated burst.
                        if (now - lastCapacityRejection >= settings.backoffResetAfter) {
                            backoffSteps = 0;
                        }
                    }
                    backoffSteps++;
                    target = Math.max(
                        1,
                        Math.min(
                            target - 1,
                            Math.ceil((target * (100 - settings.reductionPercent)) / 100),
                        ),
                    );
                    const base = Math.min(
                        settings.backoffMax,
                        settings.backoffBase * 2 ** Math.min(backoffSteps - 1, 31),
                    );
                    const jitter = 1 + random() * 0.5;
                    cooldownUntil = now + Math.min(settings.backoffMax, base * jitter);
                    startInterval = !capacityFeedback
                        ? settings.startIntervalMs / 1000
                        : Math.min(
                            settings.startIntervalMaxMs / 1000,
                            Math.max(startInterval * 2, settings.startIntervalMs / 1000),
                        );
                    capacityFeedback = true;
                    successesSinceAdjustment = 0;
                    successesSinceRate = 0;
                    lastAdjustment = now;
                } else {
                    cooldownUntil = now + 0.25;
                }
                lastCapacityRejection = now;
                nextStartAt = now + Math.max(0.01, startInterval);
                continue;
            }

            if (Number.isFinite(env.tokenRate)) {
                tokens -= 1;
            }
            startTimes.push(now);
            const load = Math.max(0, active + 1 - env.knee) / Math.max(1, env.knee);
            const baseService = env.serviceMin
                + random() * Math.max(0, env.serviceMax - env.serviceMin);
            let service = baseService * (1 + env.penalty * load);
            if (config.protocol === "h1" && sessions === 0) {
                service += 0.2;
            }
            if (config.protocol === "h2" && active % Math.max(1, config.streams) === 0) {
                service += 0.4;
            }
            if (random() < env.stallProbability) {
                service += env.stallMin + random() * Math.max(0, env.stallMax - env.stallMin);
                localRetries++;
            }
            completions.push(now + service);
            active++;
            pending--;
            nextStartAt = now + startInterval;
            continue;
        }

        const nextCompletion = completions.length ? Math.min(...completions) : Infinity;
        const nextEvent = Math.min(
            nextCompletion,
            blockedUntil > now ? blockedUntil : Infinity,
            env.capacities ? capacityAt : Infinity,
        );
        if (!Number.isFinite(nextEvent)) {
            break;
        }
        now = Math.max(now, nextEvent);

        if (env.capacities && now >= capacityAt) {
            capacityIndex = (capacityIndex + 1) % env.capacities.length;
            streamCapacity = env.capacities[capacityIndex];
            capacityAt = now + env.capacityInterval;
        }
        if (now < nextCompletion) {
            continue;
        }

        const done = completions.filter((time) => time <= now + 1e-9).length;
        completions = completions.filter((time) => time > now + 1e-9);
        for (let index = 0; index < done; index++) {
            completed++;
            active--;
            if (settings.enabled) {
                successesSinceAdjustment++;
                successesSinceRate++;
                if (
                    startInterval > settings.rateMinIntervalMs / 1000
                    && successesSinceRate >= settings.rateRecoverySuccesses
                ) {
                    startInterval = Math.max(
                        settings.rateMinIntervalMs / 1000,
                        startInterval * 0.9,
                    );
                    successesSinceRate = 0;
                }
            }
        }

        if (
            settings.enabled
            && active === 0
            && target < ceiling
            && successesSinceAdjustment >= threshold()
            && now >= cooldownUntil
            && now >= banUntil
            && now - lastAdjustment >= settings.recoveryInterval
        ) {
            target++;
            successesSinceAdjustment = 0;
            lastAdjustment = now;
        }
    }

    return {
        complete: completed === RANGE_COUNT,
        seconds: now,
        refusals,
        bans,
        localRetries,
        target,
        ceiling,
    };
}

function quantile(values, fraction) {
    if (!values.length) {
        return null;
    }
    const sorted = [...values].sort((left, right) => left - right);
    return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))];
}

function summarize(env, config) {
    const results = Array.from({ length: RUNS }, (_, index) =>
        run(env, config, (index + 1) * 7919 + env.name.length * 101));
    const completeRuns = results.filter((result) => result.complete);
    const times = completeRuns.map((result) => result.seconds);
    const average = (selector) =>
        results.reduce((sum, item) => sum + selector(item), 0) / results.length;
    return {
        completionRate: completeRuns.length / results.length,
        banRate: results.filter((result) => result.bans > 0).length / results.length,
        p50Seconds: quantile(times, 0.5),
        p95Seconds: quantile(times, 0.95),
        meanRefusals: average((item) => item.refusals),
        meanLocalRetries: average((item) => item.localRetries),
        finalTarget: average((item) => item.target),
    };
}

function score(summaries, bestByEnv) {
    let total = 0;
    let worst = 0;
    for (const [envName, summary] of summaries) {
        const best = bestByEnv.get(envName) ?? 1;
        const time = summary.p95Seconds === null
            ? 5
            : summary.p95Seconds / Math.max(1, best);
        const value = time
            + 3 * (1 - summary.completionRate)
            + 2 * summary.banRate
            + Math.min(0.5, summary.meanRefusals / 200);
        total += value;
        worst = Math.max(worst, value);
    }
    return {
        mean: total / summaries.size,
        worst,
    };
}

function evaluate(protocol) {
    const envs = ENVIRONMENTS.filter((env) => env.protocol === protocol);
    const rows = [];
    const configs = candidates(protocol).filter((config) =>
        ONLY.size === 0 || ONLY.has(config.name));
    for (const config of configs) {
        const summaries = new Map();
        for (const env of envs) {
            summaries.set(env.name, summarize(env, config));
        }
        rows.push({
            config: config.name,
            ceiling: effectiveCeiling(config),
            summaries,
        });
    }
    const bestByEnv = new Map();
    for (const row of rows) {
        for (const [envName, summary] of row.summaries) {
            const best = bestByEnv.get(envName);
            if (summary.p95Seconds !== null && (!best || summary.p95Seconds < best)) {
                bestByEnv.set(envName, summary.p95Seconds);
            }
        }
    }
    for (const row of rows) {
        const ranked = score(row.summaries, bestByEnv);
        row.meanScore = Number(ranked.mean.toFixed(3));
        row.worstScore = Number(ranked.worst.toFixed(3));
        row.envs = Object.fromEntries(row.summaries);
        delete row.summaries;
    }
    rows.sort((left, right) =>
        left.meanScore - right.meanScore || left.worstScore - right.worstScore);
    return {
        protocol,
        ranges: RANGE_COUNT,
        runs: RUNS,
        environments: envs.map((env) => env.name),
        top: rows.slice(0, TOP),
    };
}

function selfCheck() {
    const generous = ENVIRONMENTS.find((env) => env.name === "h1-generous");
    const current = candidates("h1").find((item) => item.name === "h1-x10-current");
    assert.ok(current, "the current HTTP/1.1 candidate must exist");
    const result = run(generous, current, 1);
    assert.ok(result.complete, "a generous server must complete the simulated download");
    assert.ok(result.refusals === 0, "a generous server must not produce capacity refusals");

    const cap = ENVIRONMENTS.find((env) => env.name === "h1-connection-cap-4");
    const overCapConfig = candidates("h1").find((item) => item.name === "h1-x16-off");
    assert.ok(overCapConfig, "the over-cap HTTP/1.1 candidate must exist");
    const overCap = run(cap, overCapConfig, 3);
    assert.ok(overCap.refusals > 0, "exceeding the configured connection cap must be rejected");

    const h2 = ENVIRONMENTS.find((env) => env.name === "h2-session-cap-2");
    const h2Config = candidates("h2").find((item) => item.name === "h2-s4-n4-off");
    assert.ok(h2Config, "the HTTP/2 over-cap candidate must exist");
    assert.ok(run(h2, h2Config, 5).refusals > 0, "exceeding HTTP/2 sessions must be rejected");
}

selfCheck();
for (const protocol of ["h1", "h2"]) {
    console.log(JSON.stringify(evaluate(protocol)));
}
