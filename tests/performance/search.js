/**
 * k6 load test for POST /api/v1/tenants/{tenant_id}/search
 *
 * Measures HTTP latency with the fake embedder (provider cost excluded),
 * so the numbers reflect the system's own cost (S12).
 *
 * Usage:
 *   k6 run --env TENANT_ID=<uuid> --env TOKEN=<bearer> \
 *          --env BASE_URL=http://localhost:8080 \
 *          --vus 10 --duration 60s tests/performance/search.js
 *
 * Or use the three-stage executor defined in `options` below (10, 50, 100 VUs).
 *
 * Environment variables:
 *   BASE_URL   - API base URL (default: http://localhost:8080)
 *   TENANT_ID  - UUID of the bench tenant (required)
 *   TOKEN      - Bearer token for a member of TENANT_ID (required)
 */

import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const BASE_URL = __ENV.BASE_URL || 'http://localhost:8080';
const TENANT_ID = __ENV.TENANT_ID;
const TOKEN = __ENV.TOKEN;

if (!TENANT_ID) {
  throw new Error('TENANT_ID env var is required');
}
if (!TOKEN) {
  throw new Error('TOKEN env var is required');
}

// Three-stage executor matching the plan: 10, 50, 100 VUs for 60 s each
export const options = {
  scenarios: {
    vus_10: {
      executor: 'constant-vus',
      vus: 10,
      duration: '60s',
      startTime: '0s',
      tags: { stage: '10vus' },
    },
    vus_50: {
      executor: 'constant-vus',
      vus: 50,
      duration: '60s',
      startTime: '65s',
      tags: { stage: '50vus' },
    },
    vus_100: {
      executor: 'constant-vus',
      vus: 100,
      duration: '60s',
      startTime: '130s',
      tags: { stage: '100vus' },
    },
  },
  thresholds: {
    // P95 < 500 ms is a soft target; adjust once real numbers are in
    'http_req_duration{stage:10vus}': ['p(95)<500'],
    'http_req_duration{stage:50vus}': ['p(95)<500'],
    'http_req_duration{stage:100vus}': ['p(95)<500'],
    'http_req_failed': ['rate<0.01'],
  },
};

// A small set of representative keyword queries
const QUERIES = [
  { query: 'SKU-1000', limit: 10 },
  { query: 'policy compliance retention', limit: 10 },
  { query: 'authentication token session', limit: 10 },
  { query: 'backup recovery incident schedule', limit: 10 },
  { query: 'deployment configuration environment', limit: 5 },
  { query: 'encryption certificate security access', limit: 10 },
  { query: 'approval workflow request response', limit: 10 },
  { query: 'threshold metric alert notification', limit: 10 },
];

const headers = {
  'Content-Type': 'application/json',
  Authorization: `Bearer ${TOKEN}`,
};

const url = `${BASE_URL}/api/v1/tenants/${TENANT_ID}/search`;

export default function () {
  const q = QUERIES[Math.floor(Math.random() * QUERIES.length)];
  const payload = JSON.stringify(q);

  const res = http.post(url, payload, { headers, tags: { name: 'search' } });

  check(res, {
    'status 200': (r) => r.status === 200,
    'has results field': (r) => {
      try {
        return Array.isArray(JSON.parse(r.body).results);
      } catch {
        return false;
      }
    },
  });

  // No sleep: we want to max out throughput per VU
}
