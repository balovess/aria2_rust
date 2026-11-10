#!/usr/bin/env node

// Offline policy simulator for HTTP Range concurrency. This is a synthetic
// model, not a prediction for any particular origin. Keep the event classes
// separate: a local zero-speed retry is not a server capacity rejection.

import assert from "node:assert/strict";

const CEILING = Number(process.env.SIM_CEILING ?? 16);
const RANGE_COUNT = Math.max(256, CEILING * 4);
const MAX_SECONDS = 24 * 60 * 60;
assert.ok(Number.isInteger(CEILING) && CEILING >= 1);

const halfCapacity = Math.max(1, Math.floor(CEILING / 2));
const ratePerSecond = 4;
const tokenBurst = 2;

class Random {
    constructor(seed) {
        this.state = seed >>> 0;
    }

    next() {
        this.state = (1664525 * this.state + 1013904223) >>> 0;
        return this.state / 0x1_0000_0000;
    }

    between(min, max) {
        return min + (max - min) * this.next();
    }
}

const environments = [
    {
        name: "stall-recovery",
        capacity: CEILING,
        serviceMinSeconds: 10,
        serviceMaxSeconds: 30,
        stallProbability: 0.25,
        stallMinSeconds: 2,
        stallMaxSeconds: 12,
    },
    {
        name: "stable-half-capacity",
        capacity: halfCapacity,
        serviceMinSeconds: 15,
        serviceMaxSeconds: 45,
        banRefusals: 8,
        banWindowSeconds: 60,
        cooldownSeconds: 10,
    },
    {
        name: "stable-fixed-capacity",
        capacity: 8,
        serviceMinSeconds: 15,
        serviceMaxSeconds: 45,
        banRefusals: 8,
        banWindowSeconds: 60,
        cooldownSeconds: 10,
    },
    {
        name: "rate-limit-token-bucket",
        capacity: CEILING,
        serviceMinSeconds: 5,
        serviceMaxSeconds: 20,
        tokensPerSecond: ratePerSecond,
        tokenBurst,
        banRefusals: 8,
        banWindowSeconds: 120,
        cooldownSeconds: 30,
    },
    {
        name: "changing-capacity",
        serviceMinSeconds: 8,
        serviceMaxSeconds: 40,
        capacities: [0.125, 0.25, 0.5, 0.75, 1].map((fraction) =>
            Math.max(1, Math.floor(CEILING * fraction)),
        ),
        capacityChangeSeconds: 45,
        banRefusals: 10,
        banWindowSeconds: 120,
        cooldownSeconds: 20,
    },
    {
        name: "waf-burst-window",
        capacity: CEILING,
        serviceMinSeconds: 1,
        serviceMaxSeconds: 5,
        startsPerWindow: 3,
        startWindowSeconds: 2,
        banRefusals: 3,
        banWindowSeconds: 180,
        cooldownSeconds: 45,
    },
];

const policies = [
    {
        name: "current-accepted-count-drop",
        drop: "accepted",
        cooldownSeconds: 0,
        probeSuccesses: 8,
        probeIntervalSeconds: 5,
        startGapSeconds: 0,
    },
    {
        name: "one-step-5s",
        drop: "one",
        cooldownSeconds: 5,
        probeSuccesses: 8,
        probeIntervalSeconds: 5,
        startGapSeconds: 0,
    },
    {
        name: "one-step-20s",
        drop: "one",
        cooldownSeconds: 20,
        probeSuccesses: 8,
        probeIntervalSeconds: 10,
        startGapSeconds: 0.25,
    },
    {
        name: "one-step-45s",
        drop: "one",
        cooldownSeconds: 45,
        probeSuccesses: 8,
        probeIntervalSeconds: 20,
        startGapSeconds: 0.25,
    },
    {
        name: "paced-rate-ramp",
        drop: "one",
        cooldownSeconds: 20,
        probeSuccesses: 8,
        probeIntervalSeconds: 10,
        startGapSeconds: 0.5,
        adaptRate: true,
    },
    {
        name: "paced-fast-ramp",
        drop: "one",
        cooldownSeconds: 20,
        probeSuccesses: 8,
        probeIntervalSeconds: 10,
        startGapSeconds: 0.25,
        adaptRate: true,
    },
    {
        name: "reactive-rate-ramp",
        drop: "one",
        cooldownSeconds: 20,
        probeSuccesses: 8,
        probeIntervalSeconds: 10,
        startGapSeconds: 0,
        adaptRate: true,
        rateRampSuccesses: 64,
    },
    {
        name: "reactive-multiplicative-clamped-10s",
        drop: "multiplicative",
        dropRetention: 0.8,
        cooldownSeconds: 20,
        probeSuccesses: "target-clamped",
        probeIntervalSeconds: 10,
        startGapSeconds: 0.1,
        adaptRate: true,
        rateRampSuccesses: 64,
    },
    {
        name: "reactive-multiplicative-25-clamped-10s",
        drop: "multiplicative",
        dropRetention: 0.75,
        cooldownSeconds: 20,
        probeSuccesses: "target-clamped",
        probeIntervalSeconds: 10,
        startGapSeconds: 0.1,
        adaptRate: true,
        rateRampSuccesses: 64,
    },
    {
        name: "reactive-multiplicative-50-clamped-10s",
        drop: "multiplicative",
        dropRetention: 0.5,
        cooldownSeconds: 20,
        probeSuccesses: "target-clamped",
        probeIntervalSeconds: 10,
        startGapSeconds: 0.1,
        adaptRate: true,
        rateRampSuccesses: 64,
    },
    ...[8, 16, 32, "2x-target"].flatMap((successes) => [5, 10].map((seconds) => ({
        name: `reactive-${successes}-success-${seconds}s`,
        drop: "one",
        cooldownSeconds: 20,
        probeSuccesses: successes,
        probeIntervalSeconds: seconds,
        startGapSeconds: 0,
        adaptRate: true,
        rateRampSuccesses: 64,
    }))),
    ...[5, 10].map((seconds) => ({
        name: `reactive-target-clamped-${seconds}s`,
        drop: "one",
        cooldownSeconds: 20,
        probeSuccesses: "target-clamped",
        probeIntervalSeconds: seconds,
        startGapSeconds: 0,
        adaptRate: true,
        rateRampSuccesses: 64,
    })),
];

function sampleCapacity(environment, random, now, state) {
    if (!environment.capacities) {
        return environment.capacity;
    }
    if (now >= state.nextCapacityChange) {
        state.capacity = environment.capacities[
            Math.floor(random.next() * environment.capacities.length)
        ];
        state.nextCapacityChange = now + environment.capacityChangeSeconds;
    }
    return state.capacity;
}

function run(environment, policy, seed) {
    const random = new Random(seed);
    const state = { capacity: undefined, nextCapacityChange: 0 };
    let now = 0;
    let pending = RANGE_COUNT;
    let completed = 0;
    let target = CEILING;
    let successesSinceAdjustment = 0;
    let lastAdjustment = 0;
    let cooldownUntil = 0;
    let tokenCount = environment.tokenBurst ?? Infinity;
    let tokenTime = 0;
    let requests = 0;
    let localRetries = 0;
    let refusedRequests = 0;
    let deniedRequestTimes = [];
    let startTimes = [];
    let launchGap = policy.startGapSeconds;
    let successfulSinceRateChange = 0;
    let backoffSteps = 0;
    let lastRefusalAt = null;
    let banned = false;
    let iterations = 0;

    while (pending > 0 && now < MAX_SECONDS && iterations++ < 100_000) {
        if (now < cooldownUntil) {
            const elapsed = cooldownUntil - now;
            if (Number.isFinite(tokenCount)) {
                tokenCount = Math.min(
                    environment.tokenBurst,
                    tokenCount + elapsed * environment.tokensPerSecond,
                );
                tokenTime = cooldownUntil;
            }
            now = cooldownUntil;
        }

        const capacity = sampleCapacity(environment, random, now, state);
        const offered = Math.min(target, pending);
        const activeEnds = [];
        let accepted = 0;
        let roundRefusals = 0;
        let localStallDelay = 0;
        let firstRefusalAt = null;

        for (let index = 0; index < offered; index++) {
            const startAt = now + index * launchGap;
            const feedbackReadyAt = firstRefusalAt === null
                ? Infinity
                : firstRefusalAt + (environment.feedbackLatencySeconds ?? 0.15);
            if (startAt > feedbackReadyAt) {
                break;
            }
            requests++;
            for (let cursor = activeEnds.length - 1; cursor >= 0; cursor--) {
                if (activeEnds[cursor] <= startAt) {
                    activeEnds.splice(cursor, 1);
                }
            }
            startTimes = startTimes.filter(
                (previous) => startAt - previous < (environment.startWindowSeconds ?? Infinity),
            );
            const burstLimited = environment.startsPerWindow !== undefined
                && startTimes.length >= environment.startsPerWindow;
            startTimes.push(startAt);

            if (Number.isFinite(tokenCount)) {
                const elapsed = Math.max(0, startAt - tokenTime);
                tokenCount = Math.min(
                    environment.tokenBurst,
                    tokenCount + elapsed * environment.tokensPerSecond,
                );
                tokenTime = startAt;
            }

            const rateLimited = Number.isFinite(tokenCount) && tokenCount < 1;
            const capacityLimited = activeEnds.length >= capacity;
            if (burstLimited || rateLimited || capacityLimited) {
                roundRefusals++;
                refusedRequests++;
                firstRefusalAt ??= startAt;
                deniedRequestTimes.push(startAt);
                deniedRequestTimes = deniedRequestTimes.filter(
                    (time) => startAt - time < (environment.banWindowSeconds ?? Infinity),
                );
                if (environment.banRefusals !== undefined
                    && deniedRequestTimes.length >= environment.banRefusals) {
                    banned = true;
                    break;
                }
                continue;
            }

            if (Number.isFinite(tokenCount)) {
                tokenCount -= 1;
            }
            accepted++;

            let localDelay = 0;
            if (random.next() < (environment.stallProbability ?? 0)) {
                // A temporary zero-speed event retries this Range locally.
                // It adds one request and delay, but is not classified as
                // server capacity feedback and does not change the target.
                requests++;
                localRetries++;
                localDelay = random.between(
                    environment.stallMinSeconds,
                    environment.stallMaxSeconds,
                );
                localStallDelay = Math.max(localStallDelay, localDelay);
            }
            const serviceTime = random.between(
                environment.serviceMinSeconds ?? 2,
                environment.serviceMaxSeconds ?? 2,
            );
            activeEnds.push(startAt + serviceTime + localDelay);
        }

        assert.equal(completed + pending, RANGE_COUNT);
        assert.ok(accepted <= offered);
        assert.ok(target >= 1 && target <= CEILING);
        assert.ok(tokenCount >= 0 || !Number.isFinite(tokenCount));

        const roundEnd = activeEnds.length > 0
            ? Math.max(...activeEnds)
            : now + Math.max(0.25, (offered - 1) * launchGap);
        const roundDuration = Math.max(0.25, roundEnd - now);
        now += roundDuration;
        if (Number.isFinite(tokenCount)) {
            tokenCount = Math.min(
                environment.tokenBurst,
                tokenCount + roundDuration * environment.tokensPerSecond,
            );
            tokenTime = now;
        }

        completed += accepted;
        pending -= accepted;
        successesSinceAdjustment += accepted;

        if (roundRefusals > 0) {
            successesSinceAdjustment = 0;
            lastAdjustment = now;
            if (lastRefusalAt !== null && now - lastRefusalAt >= 60) {
                backoffSteps = 0;
            }
            lastRefusalAt = now;
            backoffSteps++;
            if (policy.drop === "accepted") {
                target = Math.max(1, Math.min(target - 1, accepted));
            } else if (policy.drop === "multiplicative") {
                target = Math.max(
                    1,
                    Math.min(target - 1, Math.ceil(target * (policy.dropRetention ?? 0.8))),
                );
            } else {
                target = Math.max(1, target - 1);
            }
            const backoffBase = Math.min(300, 20 * 2 ** Math.min(backoffSteps - 1, 4));
            const backoff = random.between(backoffBase, Math.min(300, backoffBase * 1.5));
            cooldownUntil = now + Math.max(
                policy.cooldownSeconds,
                environment.cooldownSeconds ?? 0,
                backoff,
            );
            if (policy.adaptRate) {
                launchGap = Math.min(4, Math.max(1, launchGap * 2));
                successfulSinceRateChange = 0;
            }
        } else if (
            target < CEILING
            && successesSinceAdjustment >= (
                policy.probeSuccesses === "2x-target"
                    ? Math.max(8, target * 2)
                    : policy.probeSuccesses === "target-clamped"
                        ? Math.min(32, Math.max(8, target))
                        : policy.probeSuccesses
            )
            && now - lastAdjustment >= policy.probeIntervalSeconds
        ) {
            target++;
            successesSinceAdjustment = 0;
            lastAdjustment = now;
        }

        if (banned) {
            break;
        }

        if (roundRefusals === 0 && policy.adaptRate && launchGap > 0) {
            successfulSinceRateChange += accepted;
            if (successfulSinceRateChange >= (policy.rateRampSuccesses ?? 32)) {
                launchGap = Math.max(0.01, launchGap * 0.9);
                successfulSinceRateChange = 0;
            }
        }
    }

    assert.equal(completed + pending, RANGE_COUNT);
    assert.ok(requests >= completed);
    assert.ok(refusedRequests <= requests);
    return {
        completed,
        complete: completed === RANGE_COUNT && !banned,
        banned,
        seconds: now,
        requests,
        localRetries,
        refusedRequests,
    };
}

function validateModel() {
    const stallOnly = run(environments[0], policies[1], 7);
    assert.equal(stallOnly.banned, false);
    assert.equal(stallOnly.refusedRequests, 0);
    assert.equal(stallOnly.completed, RANGE_COUNT);
    assert.ok(stallOnly.localRetries > 0);

    const bounded = run(environments[1], policies[1], 11);
    assert.ok(bounded.completed <= RANGE_COUNT);
    assert.ok(bounded.requests >= bounded.completed);
    assert.ok(bounded.refusedRequests <= bounded.requests);

    const wafEnvironment = environments.find((environment) =>
        environment.name === "waf-burst-window",
    );
    const paced = run(
        wafEnvironment,
        policies.find((policy) => policy.name === "reactive-multiplicative-25-clamped-10s"),
        13,
    );
    assert.equal(paced.banned, false);
    assert.ok(paced.refusedRequests <= wafEnvironment.startsPerWindow);
}

function summarize(results) {
    const completedRuns = results.filter((result) => result.complete);
    const times = completedRuns.map((result) => result.seconds).sort((a, b) => a - b);
    const average = (selector) => results.reduce((sum, item) => sum + selector(item), 0) / results.length;
    return {
        completionRate: completedRuns.length / results.length,
        banRate: results.filter((result) => result.banned).length / results.length,
        medianSeconds: times.length ? times[Math.floor(times.length / 2)] : null,
        meanRequests: average((item) => item.requests),
        meanLocalRetries: average((item) => item.localRetries),
        meanRefusedRequests: average((item) => item.refusedRequests),
    };
}

validateModel();
const runs = Number(process.env.SIM_RUNS ?? 500);
for (const environment of environments) {
    const comparisons = {};
    for (const policy of policies) {
        const results = Array.from({ length: runs }, (_, index) =>
            run(environment, policy, (index + 1) * 7919 + environments.indexOf(environment) * 101),
        );
        comparisons[policy.name] = summarize(results);
    }
    console.log(JSON.stringify({ environment: environment.name, runs, comparisons }));
}
