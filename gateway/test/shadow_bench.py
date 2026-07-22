#!/usr/bin/env python3
import asyncio
import aiohttp
import time
import json

SIDECAR_URL = "http://localhost:8000"

async def test_cache(session, prompt: str, model: str):
    payload = {
        "model": model,
        "body": {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.5
        }
    }

    # 1. Check Cache
    start_time = time.time()
    async with session.post(f"{SIDECAR_URL}/cache/check", json=payload) as resp:
        res = await resp.json()

    if res["hit"]:
        latency_ms = (time.time() - start_time) * 1000
        return True, latency_ms, 0
    else:
        # Simulate LLM Call (Dumb Routing Cost)
        await asyncio.sleep(1.5) # Simulating 1.5s latency
        cost_savings = 0.002 # Fake cost metric

        # 2. Store in Cache
        store_payload = {
            "model": model,
            "body": payload["body"],
            "response": {"choices": [{"message": {"content": "Simulated response"}}]},
            "ttl_secs": 86400
        }
        async with session.post(f"{SIDECAR_URL}/cache/store", json=store_payload) as resp:
            await resp.json()

        latency_ms = (time.time() - start_time) * 1000
        return False, latency_ms, cost_savings

async def main():
    print("🚀 Starting Shadow-Mode Cache Validation 🚀\n")

    prompts = [
        "What is the capital of France?",
        "Explain quantum computing in simple terms.",
        "What is the capital of France?", # Duplicate - should HIT
        "Explain quantum computing in simple terms.", # Duplicate - should HIT
        "How do I write a fast Rust cache?"
    ]

    async with aiohttp.ClientSession() as session:
        total_cost_saved = 0.0
        total_latency_ms_cache = 0.0
        cache_hits = 0
        total_requests = len(prompts)

        for i, prompt in enumerate(prompts):
            print(f"Request {i+1}: '{prompt}'")
            hit, latency_ms, cost_saved = await test_cache(session, prompt, "gpt-5.5")

            if hit:
                print(f"  [CACHE HIT] ⚡ Latency: {latency_ms:.2f}ms | 💵 Cost Avoided: $0.002")
                cache_hits += 1
                total_cost_saved += 0.002
                total_latency_ms_cache += latency_ms
            else:
                print(f"  [CACHE MISS] 🐌 Latency: {latency_ms:.2f}ms | 💸 API Billed")

        print("\n=== Validation Results ===")
        print(f"Total Requests: {total_requests}")
        print(f"Cache Hits: {cache_hits} ({(cache_hits/total_requests)*100:.1f}%)")
        print(f"Total Cost Saved: ${total_cost_saved:.4f}")
        if cache_hits > 0:
            print(f"Avg Cache Hit Latency: {total_latency_ms_cache/cache_hits:.2f}ms (vs ~1500ms baseline)")

if __name__ == "__main__":
    asyncio.run(main())
