# Telemetry & Cost Tracking Design

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. Telemetry/cost tracking now lives in `packages/tui-rs/src/telemetry/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


The telemetry system tracks usage metrics, calculates costs, and provides analytics for understanding LLM utilization patterns.

## Overview

Telemetry capabilities:

- **Token Counting**: Input/output token tracking per request
- **Cost Calculation**: Per-model pricing with provider rates
- **Session Analytics**: Usage aggregation per session
- **Cache Metrics**: Hit/miss tracking for tool result cache
- **Export**: JSONL and structured data export

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Telemetry Architecture                            │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                  Usage Collector                             │    │
│  │  - Token counts from provider responses                     │    │
│  │  - Duration tracking                                        │    │
│  │  - Error classification                                     │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│           ┌──────────────────┼──────────────────┐                   │
│           ▼                  ▼                  ▼                   │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐        │
│  │ Token Counter  │  │ Cost Calculator│  │ Cache Tracker  │        │
│  │ - Input tokens │  │ - Provider rates│  │ - Hit ratio    │        │
│  │ - Output tokens│  │ - Model pricing │  │ - Saved tokens │        │
│  │ - Cache read   │  │ - Currency conv │  │ - Size metrics │        │
│  └────────────────┘  └────────────────┘  └────────────────┘        │
│                              │                                       │
│                              ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                    Aggregation Layer                         │    │
│  │  - Per-session rollups                                      │    │
│  │  - Per-model statistics                                     │    │
│  │  - Time-series data                                         │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│                              ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                    Export & Display                          │    │
│  │  - TUI footer display                                       │    │
│  │  - /cost command                                            │    │
│  │  - JSONL export                                             │    │
│  └─────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

## Token Counting

### Usage Interface

```typescript
// src/telemetry.ts
interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  cacheCreationInputTokens?: number;
  cacheReadInputTokens?: number;
  totalTokens: number;
}

interface RequestMetrics {
  requestId: string;
  model: string;
  provider: string;
  usage: TokenUsage;
  durationMs: number;
  timestamp: Date;
  toolCalls?: number;
  error?: string;
}
```

### Usage Tracker

```typescript
// src/telemetry.ts
class UsageTracker {
  private requests: RequestMetrics[] = [];
  private sessionStartTime: Date = new Date();

  recordRequest(metrics: RequestMetrics): void {
    this.requests.push(metrics);
  }

  getSessionUsage(): SessionUsage {
    let totalInputTokens = 0;
    let totalOutputTokens = 0;
    let totalCacheReadTokens = 0;
    let totalRequests = 0;
    let totalErrors = 0;
    let totalDurationMs = 0;

    for (const request of this.requests) {
      totalInputTokens += request.usage.inputTokens;
      totalOutputTokens += request.usage.outputTokens;
      totalCacheReadTokens += request.usage.cacheReadInputTokens ?? 0;
      totalRequests++;
      totalDurationMs += request.durationMs;

      if (request.error) {
        totalErrors++;
      }
    }

    return {
      inputTokens: totalInputTokens,
      outputTokens: totalOutputTokens,
      cacheReadTokens: totalCacheReadTokens,
      totalTokens: totalInputTokens + totalOutputTokens,
      requestCount: totalRequests,
      errorCount: totalErrors,
      averageDurationMs: totalRequests > 0
        ? totalDurationMs / totalRequests
        : 0,
      sessionDurationMs: Date.now() - this.sessionStartTime.getTime()
    };
  }

  getUsageByModel(): Map<string, SessionUsage> {
    const byModel = new Map<string, RequestMetrics[]>();

    for (const request of this.requests) {
      const key = `${request.provider}/${request.model}`;
      const existing = byModel.get(key) ?? [];
      existing.push(request);
      byModel.set(key, existing);
    }

    const result = new Map<string, SessionUsage>();
    for (const [model, requests] of byModel) {
      result.set(model, this.aggregateRequests(requests));
    }

    return result;
  }

  private aggregateRequests(requests: RequestMetrics[]): SessionUsage {
    // ... aggregation logic
  }
}
```

## Cost Calculation

### Pricing Configuration

```typescript
// src/telemetry.ts
interface ModelPricing {
  inputPerMillion: number;   // $ per 1M input tokens
  outputPerMillion: number;  // $ per 1M output tokens
  cacheReadPerMillion?: number;  // $ per 1M cache read tokens
  cacheWritePerMillion?: number; // $ per 1M cache creation tokens
}

const MODEL_PRICING: Record<string, ModelPricing> = {
  "anthropic/claude-opus-4-6": {
    inputPerMillion: 5.00,
    outputPerMillion: 25.00,
    cacheReadPerMillion: 0.50,
    cacheWritePerMillion: 6.25
  },
  "anthropic/claude-sonnet-4-5-20250929": {
    inputPerMillion: 3.00,
    outputPerMillion: 15.00,
    cacheReadPerMillion: 0.30,
    cacheWritePerMillion: 3.75
  },
  "openai/gpt-4o": {
    inputPerMillion: 2.50,
    outputPerMillion: 10.00
  },
  "openai/gpt-4o-mini": {
    inputPerMillion: 0.15,
    outputPerMillion: 0.60
  },
  "google/gemini-2.0-flash": {
    inputPerMillion: 0.075,
    outputPerMillion: 0.30
  }
};
```

### Cost Calculator

```typescript
// src/telemetry.ts
class CostCalculator {
  calculateRequestCost(metrics: RequestMetrics): number {
    const pricing = this.getPricing(metrics.provider, metrics.model);
    if (!pricing) return 0;

    const inputCost = (metrics.usage.inputTokens / 1_000_000)
                    * pricing.inputPerMillion;
    const outputCost = (metrics.usage.outputTokens / 1_000_000)
                     * pricing.outputPerMillion;

    let cacheReadCost = 0;
    if (pricing.cacheReadPerMillion && metrics.usage.cacheReadInputTokens) {
      cacheReadCost = (metrics.usage.cacheReadInputTokens / 1_000_000)
                    * pricing.cacheReadPerMillion;
    }

    let cacheWriteCost = 0;
    if (pricing.cacheWritePerMillion && metrics.usage.cacheCreationInputTokens) {
      cacheWriteCost = (metrics.usage.cacheCreationInputTokens / 1_000_000)
                     * pricing.cacheWritePerMillion;
    }

    return inputCost + outputCost + cacheReadCost + cacheWriteCost;
  }

  calculateSessionCost(tracker: UsageTracker): SessionCost {
    let totalCost = 0;
    let costByModel = new Map<string, number>();

    for (const request of tracker.getRequests()) {
      const cost = this.calculateRequestCost(request);
      totalCost += cost;

      const modelKey = `${request.provider}/${request.model}`;
      costByModel.set(modelKey, (costByModel.get(modelKey) ?? 0) + cost);
    }

    // Calculate savings from cache
    const usage = tracker.getSessionUsage();
    const cacheSavings = this.calculateCacheSavings(usage);

    return {
      totalCost,
      costByModel,
      cacheSavings,
      netCost: totalCost - cacheSavings
    };
  }

  private calculateCacheSavings(usage: SessionUsage): number {
    // Cache reads cost ~90% less than regular input tokens
    // Calculate what we would have paid without caching
    const avgInputRate = 5.00;  // Average across models
    const cacheReadRate = 0.50;

    const savedTokens = usage.cacheReadTokens;
    const wouldHavePaid = (savedTokens / 1_000_000) * avgInputRate;
    const actuallyPaid = (savedTokens / 1_000_000) * cacheReadRate;

    return wouldHavePaid - actuallyPaid;
  }

  private getPricing(provider: string, model: string): ModelPricing | null {
    // Try exact match first
    const exact = MODEL_PRICING[`${provider}/${model}`];
    if (exact) return exact;

    // Try prefix match for model families
    for (const [key, pricing] of Object.entries(MODEL_PRICING)) {
      if (`${provider}/${model}`.startsWith(key.replace(/-\d+$/, ""))) {
        return pricing;
      }
    }

    return null;
  }
}
```

## Cache Metrics

### Cache Hit Tracking

```typescript
// src/telemetry.ts
interface CacheMetrics {
  hits: number;
  misses: number;
  evictions: number;
  bytesSaved: number;
  entriesCount: number;
}

class CacheTracker {
  private metrics: CacheMetrics = {
    hits: 0,
    misses: 0,
    evictions: 0,
    bytesSaved: 0,
    entriesCount: 0
  };

  recordHit(bytes: number): void {
    this.metrics.hits++;
    this.metrics.bytesSaved += bytes;
  }

  recordMiss(): void {
    this.metrics.misses++;
  }

  recordEviction(bytes: number): void {
    this.metrics.evictions++;
    this.metrics.entriesCount--;
  }

  recordEntry(bytes: number): void {
    this.metrics.entriesCount++;
  }

  getMetrics(): CacheMetrics {
    return { ...this.metrics };
  }

  getHitRatio(): number {
    const total = this.metrics.hits + this.metrics.misses;
    return total > 0 ? this.metrics.hits / total : 0;
  }
}
```

## Session Analytics

### Analytics Data

```typescript
interface SessionAnalytics {
  // Token usage
  totalTokens: number;
  inputTokens: number;
  outputTokens: number;
  tokensPerMinute: number;

  // Cost
  totalCost: number;
  costPerRequest: number;
  cacheSavings: number;

  // Performance
  averageLatencyMs: number;
  p95LatencyMs: number;
  requestsPerMinute: number;

  // Quality
  errorRate: number;
  toolCallSuccessRate: number;

  // Model distribution
  modelUsage: Array<{
    model: string;
    requests: number;
    tokens: number;
    cost: number;
  }>;
}

function computeSessionAnalytics(tracker: UsageTracker): SessionAnalytics {
  const usage = tracker.getSessionUsage();
  const costResult = costCalculator.calculateSessionCost(tracker);
  const requests = tracker.getRequests();

  // Calculate latency percentiles
  const latencies = requests.map(r => r.durationMs).sort((a, b) => a - b);
  const p95Index = Math.floor(latencies.length * 0.95);

  // Calculate tokens per minute
  const sessionMinutes = usage.sessionDurationMs / 60000;
  const tokensPerMinute = sessionMinutes > 0
    ? usage.totalTokens / sessionMinutes
    : 0;

  // Model distribution
  const byModel = tracker.getUsageByModel();
  const modelUsage = Array.from(byModel.entries()).map(([model, modelUsage]) => ({
    model,
    requests: modelUsage.requestCount,
    tokens: modelUsage.totalTokens,
    cost: costResult.costByModel.get(model) ?? 0
  }));

  return {
    totalTokens: usage.totalTokens,
    inputTokens: usage.inputTokens,
    outputTokens: usage.outputTokens,
    tokensPerMinute,
    totalCost: costResult.totalCost,
    costPerRequest: usage.requestCount > 0
      ? costResult.totalCost / usage.requestCount
      : 0,
    cacheSavings: costResult.cacheSavings,
    averageLatencyMs: usage.averageDurationMs,
    p95LatencyMs: latencies[p95Index] ?? 0,
    requestsPerMinute: sessionMinutes > 0
      ? usage.requestCount / sessionMinutes
      : 0,
    errorRate: usage.requestCount > 0
      ? usage.errorCount / usage.requestCount
      : 0,
    toolCallSuccessRate: calculateToolCallSuccessRate(requests),
    modelUsage
  };
}
```

## Display Components

> **Historical note:** `src/cli-tui/...` paths below refer to the removed TypeScript
> TUI (PR #2891). Native cost/usage UI lives under `packages/tui-kit-rs/src/usage/` and
> related status components.

### TUI Footer Cost Display

```typescript
// historical: src/cli-tui/status/cost-view.ts
class CostView {
  render(analytics: SessionAnalytics): string {
    const costStr = formatCurrency(analytics.totalCost);
    const tokensStr = formatNumber(analytics.totalTokens);
    const savingsStr = analytics.cacheSavings > 0
      ? ` (saved ${formatCurrency(analytics.cacheSavings)})`
      : "";

    return `Cost: ${costStr}${savingsStr} │ Tokens: ${tokensStr}`;
  }
}

function formatCurrency(amount: number): string {
  if (amount < 0.01) {
    return `$${(amount * 100).toFixed(2)}¢`;
  }
  return `$${amount.toFixed(4)}`;
}

function formatNumber(n: number): string {
  if (n >= 1_000_000) {
    return `${(n / 1_000_000).toFixed(2)}M`;
  }
  if (n >= 1_000) {
    return `${(n / 1_000).toFixed(1)}K`;
  }
  return n.toString();
}
```

### /cost Command

```typescript
// src/cli-tui/commands/cost-handlers.ts
async function handleCostCommand(): Promise<void> {
  const analytics = computeSessionAnalytics(usageTracker);

  console.log("\n📊 Session Cost Summary\n");
  console.log(`Total Cost:     ${formatCurrency(analytics.totalCost)}`);
  console.log(`Cache Savings:  ${formatCurrency(analytics.cacheSavings)}`);
  console.log(`Net Cost:       ${formatCurrency(analytics.totalCost - analytics.cacheSavings)}`);
  console.log();
  console.log(`📈 Token Usage\n`);
  console.log(`Input Tokens:   ${formatNumber(analytics.inputTokens)}`);
  console.log(`Output Tokens:  ${formatNumber(analytics.outputTokens)}`);
  console.log(`Total Tokens:   ${formatNumber(analytics.totalTokens)}`);
  console.log(`Tokens/min:     ${analytics.tokensPerMinute.toFixed(1)}`);
  console.log();
  console.log(`⚡ Performance\n`);
  console.log(`Avg Latency:    ${analytics.averageLatencyMs.toFixed(0)}ms`);
  console.log(`P95 Latency:    ${analytics.p95LatencyMs.toFixed(0)}ms`);
  console.log(`Error Rate:     ${(analytics.errorRate * 100).toFixed(1)}%`);
  console.log();
  console.log(`🤖 Model Usage\n`);

  for (const model of analytics.modelUsage) {
    console.log(`${model.model}:`);
    console.log(`  Requests: ${model.requests}`);
    console.log(`  Tokens:   ${formatNumber(model.tokens)}`);
    console.log(`  Cost:     ${formatCurrency(model.cost)}`);
  }
}
```

## Export

### JSONL Export

```typescript
// src/telemetry.ts
function exportTelemetryJsonl(tracker: UsageTracker): string {
  const lines: string[] = [];

  // Export individual requests
  for (const request of tracker.getRequests()) {
    lines.push(JSON.stringify({
      type: "request",
      ...request
    }));
  }

  // Export session summary
  const analytics = computeSessionAnalytics(tracker);
  lines.push(JSON.stringify({
    type: "session_summary",
    ...analytics
  }));

  return lines.join("\n");
}
```

### Structured Export

```typescript
interface TelemetryExport {
  version: string;
  exportedAt: string;
  session: {
    id: string;
    startTime: string;
    endTime: string;
  };
  summary: SessionAnalytics;
  requests: RequestMetrics[];
  cacheMetrics: CacheMetrics;
}

function exportTelemetry(tracker: UsageTracker): TelemetryExport {
  return {
    version: "1.0",
    exportedAt: new Date().toISOString(),
    session: {
      id: sessionManager.getSessionId(),
      startTime: tracker.getStartTime().toISOString(),
      endTime: new Date().toISOString()
    },
    summary: computeSessionAnalytics(tracker),
    requests: tracker.getRequests(),
    cacheMetrics: cacheTracker.getMetrics()
  };
}
```

## Privacy Considerations

```typescript
// src/telemetry.ts
interface TelemetryConfig {
  enabled: boolean;
  collectRequestMetrics: boolean;
  collectCacheMetrics: boolean;
  includeModelNames: boolean;
  includeToolNames: boolean;
  anonymizeSession: boolean;
}

function sanitizeForExport(
  data: TelemetryExport,
  config: TelemetryConfig
): TelemetryExport {
  const sanitized = { ...data };

  if (config.anonymizeSession) {
    sanitized.session.id = hashString(data.session.id);
  }

  if (!config.includeModelNames) {
    sanitized.requests = data.requests.map(r => ({
      ...r,
      model: "redacted",
      provider: "redacted"
    }));
  }

  return sanitized;
}
```

## Related Documentation

- [Agent State Machine](AGENT_STATE_MACHINE.md) - Request tracking
- [Session Persistence](SESSION_PERSISTENCE.md) - Usage per session
- [Enterprise RBAC](ENTERPRISE_RBAC.md) - Quota tracking
