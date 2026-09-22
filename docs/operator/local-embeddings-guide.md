# Local Embeddings Setup Guide for CoNexus

**A Beginner-Friendly Guide to Using Free Local Embedding Models**

---

## 📋 Table of Contents
1. [What Are Embeddings?](#what-are-embeddings)
2. [Why Use Local Models?](#why-use-local-models)
3. [Prerequisites](#prerequisites)
4. [Installing Ollama](#installing-ollama)
5. [Pulling Embedding Models](#pulling-embedding-models)
6. [Configuring CoNexus](#configuring-conexus)
7. [Testing Your Setup](#testing-your-setup)
8. [Performance Benchmarks](#performance-benchmarks)
9. [Troubleshooting](#troubleshooting)
10. [FAQ](#faq)

---

## What Are Embeddings?

**In Simple Terms**: Embeddings are a way to convert text into numbers that computers can understand and compare.

Think of it like this: If you want to know how similar "cat" and "dog" are, you can't just compare the letters. But if you convert them into numbers (vectors) based on their meaning, you can mathematically calculate how similar they are.

**Example**:
```
"cat" → [0.5, 0.3, 0.1, ...] (1536 numbers)
"dog" → [0.6, 0.4, 0.2, ...] (1536 numbers)
"car" → [0.1, 0.9, 0.7, ...] (1536 numbers)
```

When you compare these vectors, "cat" and "dog" would be very similar (high score), while "cat" and "car" would be less similar (lower score).

**Why CoNexus Uses Embeddings**:
- Search through your project documentation intelligently
- Find relevant code snippets
- Understand relationships between tasks
- Power the RAG (Retrieval-Augmented Generation) system

---

## Why Use Local Models?

### Cost Comparison

| Provider | Cost per 1M tokens | Monthly (typical) |
|----------|-------------------|-------------------|
| OpenAI (text-embedding-3-large) | $0.13 | $10-50+ |
| **Ollama (local)** | **$0.00** | **$0.00** |

### Benefits of Local Embeddings

1. **💰 Zero Cost**: No API fees, ever
2. **🔒 Privacy**: Your code never leaves your machine
3. **⚡ Speed**: ~37ms per text (faster than API calls)
4. **📡 Offline**: Works without internet connection
5. **🎯 Control**: Full control over model selection

### When to Use OpenAI vs Local

**Use OpenAI if:**
- You're just testing CoNexus briefly
- You already have OpenAI credits
- You need absolute best quality (minor difference)

**Use Local Models if:**
- You want to save money long-term ✅
- You care about privacy ✅
- You have decent hardware (8GB+ RAM) ✅
- You're doing development work ✅

---

## Prerequisites

### System Requirements

- **OS**: Linux, macOS, or Windows (WSL2)
- **RAM**: 8GB minimum (16GB recommended)
- **Disk Space**: 2GB for models
- **CPU**: Modern multi-core processor

### Already Installed?

Check if you have Ollama:
```bash
which ollama
# or on Windows:
where ollama
```

If you see a path, skip to [Pulling Embedding Models](#pulling-embedding-models).

---

## Installing Ollama

### Linux & macOS

```bash
curl -fsSL https://ollama.ai/install.sh | sh
```

### Windows

1. Download from: https://ollama.ai/download/windows
2. Run the installer
3. Open PowerShell or CMD

### Verify Installation

```bash
ollama --version
```

You should see something like: `ollama version 0.x.x`

### Start Ollama Service

Ollama runs as a background service:

```bash
# On Linux/macOS (usually starts automatically)
ollama serve

# On Windows (runs automatically as a service)
# No action needed
```

**Tip**: Leave the `ollama serve` command running in a separate terminal window.

---

## Pulling Embedding Models

### Recommended Model: Qwen3-Embedding 0.6B

**Why Qwen3?** (Recommended by LR, CoNexus maintainer)
- ✅ Best balance of speed and quality
- ✅ Small size (640MB)
- ✅ Fast inference (~37ms per text)
- ✅ 1024-dimensional embeddings (auto-normalized to 1536)

### Pull the Model

```bash
ollama pull qwen3-embedding:0.6b
```

**What's happening:**
- Downloads the model (~640MB)
- Usually takes 1-5 minutes depending on your internet
- Only needs to be done once

### Verify Model is Available

```bash
ollama list
```

You should see:
```
NAME                     ID              SIZE      MODIFIED
qwen3-embedding:0.6b     ac6da0dfba84    640 MB    X minutes ago
```

### Alternative Models

If you want to try other models:

```bash
# Smaller, faster (but less accurate)
ollama pull nomic-embed-text        # 768D, 274MB

# Larger, better quality (but slower)
ollama pull mxbai-embed-large       # 1024D, 670MB
```

---

## Configuring CoNexus

The Rust `conexus-backend`/`conexus-router` binaries read real process
environment variables only — there is no `.env`-file-loading mechanism
(the old Python single-process server's `.env` support did not carry
over). Set these however your deployment already sets other env vars:
plain `export` in a dev shell, a systemd `Environment=`/
`EnvironmentFile=` line, or (for the Nix/home-manager deployment path)
the `services.conexus.*` module options.

The provider switch is presence/absence of a non-empty `OPENAI_API_KEY`
— leave it **unset entirely** and Ollama is used; set it to a real
OpenAI key and OpenAI is used instead. `OPENAI_API_KEY` is NOT a
sentinel you set to `"ollama"` — any non-empty value (including the
literal string `"ollama"`) routes through the OpenAI branch, which
reads a completely different set of variables (`OPENAI_BASE_URL`, not
`CONEXUS_LLM_BASE_URL`) and would silently try to reach OpenAI's
real cloud endpoint with a garbage key. There is no separate
`EMBEDDING_PROVIDER`/`EMBEDDING_PROVIDERS` fallback-chain variable —
this is a single either/or switch, not a chain (see
[`docs/operator/getting-started.md`](getting-started.md#environment-variables)
for the authoritative table).

### Step 1: Set the local-Ollama variables

```bash
unset OPENAI_API_KEY                                    # must be UNSET, not "ollama" — see above
export CONEXUS_LLM_BASE_URL=http://localhost:11434/v1  # Ollama's OpenAI-compatible endpoint
export CONEXUS_EMBEDDING_MODEL=qwen3-embedding:0.6b
export CONEXUS_EMBEDDING_DIMENSION=1024               # must match the embedding model
```

### Step 2: Verify Configuration

Start `conexus-router` (or `conexus-backend` directly, for local
debugging) with those variables exported and confirm a RAG query
against a real project succeeds — there is no separate `.env` file to
inspect.

**Important**:
- CoNexus itself doesn't need an Anthropic key — that's your MCP
  client's (e.g. Claude Code's) own concern, not this server's.
- Only embeddings/RAG run through this switch; it has no effect on
  which chat model your MCP client uses for its own reasoning.

---

## Testing Your Setup

### Quick Test: Direct Ollama API

Test if Ollama is working:

```bash
curl http://localhost:11434/api/embeddings -d '{
  "model": "qwen3-embedding:0.6b",
  "prompt": "Hello world"
}' | jq '.embedding | length'
```

**Expected output**: `1024` (dimension count)

### Full Test: CoNexus Integration

Once configured, start `conexus-router` normally (with the variables
from Step 1 exported) and confirm RAG queries succeed against a real
project.

**If queries succeed**: You're all set!

**If something fails**: See [Troubleshooting](#troubleshooting) below

---

## Performance Benchmarks

### Real-World Performance (Tested on qwen3-embedding:0.6b)

| Operation | Time | Notes |
|-----------|------|-------|
| Cold start (warmup) | 54ms | First embedding |
| Single text | 53ms | After warmup |
| Batch (3 texts) | 109ms | 36.3ms avg per text |
| Batch (10 texts) | 375ms | 37.5ms avg per text |
| **Throughput** | **~27 texts/sec** | Consistent |

### Comparison with OpenAI

| Metric | Ollama (Local) | OpenAI (Cloud) |
|--------|----------------|----------------|
| Average latency | 37ms | 200-500ms |
| Cost per 1M tokens | $0.00 | $0.13 |
| Requires internet | No | Yes |
| Privacy | 100% local | Data sent to OpenAI |
| Quality | Excellent | Excellent+ |

**Bottom Line**: Local is faster AND free, with minimal quality difference.

---

## Troubleshooting

### Issue #1: "Ollama not available"

**Symptoms**:
```
❌ Provider is not available
⚠️ Make sure Ollama is running
```

**Solutions**:

1. **Check if Ollama is running**:
   ```bash
   curl http://localhost:11434/api/tags
   ```

   - **If it fails**: Start Ollama
     ```bash
     ollama serve
     ```

2. **Check if the service is on a different port**:
   ```bash
   # Try the health endpoint
   curl http://localhost:11434/
   ```

3. **Restart Ollama**:
   ```bash
   # Kill existing process
   pkill ollama

   # Start fresh
   ollama serve
   ```

### Issue #2: "Model not found"

**Symptoms**:
```
Ollama model qwen3-embedding:0.6b not found
Pull it with: ollama pull qwen3-embedding:0.6b
```

**Solution**:
```bash
# Pull the model
ollama pull qwen3-embedding:0.6b

# Verify it's installed
ollama list
```

### Issue #3: Slow performance

**Possible causes**:

1. **First run (model loading)**:
   - Solution: Wait for warmup, subsequent calls will be fast

2. **Limited RAM**:
   - Check RAM usage: `htop` or `top`
   - Consider using smaller model: `nomic-embed-text`

3. **CPU throttling**:
   - Check CPU: `htop`
   - Make sure laptop is plugged in (not on battery saver)

### Issue #4: "Dimension mismatch"

**Symptoms**:
```
Expected 1536 dimensions, got 1024
```

**This is actually NORMAL**:
- Qwen3-embedding produces 1024D vectors
- CoNexus automatically pads them to 1536D
- The test should show: ✅ Padding detected

**If you see this as an error**, the auto-padding might not be working. Check that you're using the latest version of the code.

### Issue #5: Port already in use

**Symptoms**:
```
Error: bind: address already in use
```

**Solution**:
```bash
# Find what's using port 11434
lsof -i :11434

# Kill the process
kill -9 <PID>

# Restart Ollama
ollama serve
```

---

## FAQ

### Q: Do I still need an OpenAI API key?

**A**: No, not for embeddings — CoNexus itself doesn't touch your
MCP client's own LLM key (e.g. Claude Code's `ANTHROPIC_API_KEY`,
which is that client's concern, not this server's).

### Q: Can I use both local and OpenAI embeddings, with a fallback?

**A**: No — the provider switch (`OPENAI_API_KEY` set vs. unset) is a
single either/or choice, not a fallback chain. There is no
`EMBEDDING_PROVIDERS`-style variable; if Ollama is down, requests
fail rather than silently falling back to OpenAI.

### Q: Which model is best for me?

**Recommendations**:
- **Most users**: `qwen3-embedding:0.6b` (best balance)
- **Limited RAM (<8GB)**: `nomic-embed-text` (smaller, faster)
- **Quality-focused**: `mxbai-embed-large` (larger, slower)

### Q: How much disk space do I need?

- Ollama itself: ~200MB
- Qwen3-embedding:0.6b: ~640MB
- Nomic-embed-text: ~274MB
- Total: ~1-2GB for a typical setup

### Q: Can I run this on a Mac M1/M2/M3?

**A**: Yes! Ollama has excellent Apple Silicon support:
```bash
# Install on macOS
curl -fsSL https://ollama.ai/install.sh | sh

# Same steps as above
ollama pull qwen3-embedding:0.6b
```

Performance is even better on M-series chips!

### Q: What if I'm behind a corporate proxy?

**A**: Set proxy environment variables:
```bash
export HTTP_PROXY=http://proxy.company.com:8080
export HTTPS_PROXY=http://proxy.company.com:8080

ollama pull qwen3-embedding:0.6b
```

### Q: Can I use GPU acceleration?

**A**: Ollama automatically uses GPU if available:
- **NVIDIA**: CUDA support (automatic)
- **AMD**: ROCm support (automatic)
- **Apple Silicon**: Metal (automatic)
- **CPU-only**: Works fine, just slightly slower

### Q: How do I update the model?

**A**:
```bash
# Pull the latest version
ollama pull qwen3-embedding:0.6b

# Old version is automatically replaced
```

### Q: Can I use multiple models simultaneously?

**A**: Yes, but one at a time per CoNexus instance. Change `OLLAMA_MODEL` in `.env` to switch models.

### Q: What's the quality difference vs OpenAI?

**A**:
- OpenAI: ~95% accuracy (subjective)
- Qwen3-embedding: ~92% accuracy (subjective)
- **For most use cases**: The difference is negligible
- **For critical search**: OpenAI might have a slight edge

---

## Next Steps

Once your local embeddings are working:

1. **Start the router** with the local-embedding variables exported
   (see [Getting Started](getting-started.md) for the full quick-start):
   ```bash
   conexus-router --port 5454 --projects-file projects.json --sock-dir /tmp/conexus-sockets --dashboard-dir ./result-dashboard/share/conexus-dashboard
   ```

2. **Test RAG functionality**: Try searching your codebase through the CoNexus dashboard

3. **Monitor performance**: Watch `journalctl`/stderr for embedding generation times

4. **Experiment with models**: Try different models to find your sweet spot

---

## Additional Resources

- **Ollama Documentation**: https://ollama.ai/docs
- **CoNexus README**: [README.md](../../README.md)

---

## Feedback & Support

Found an issue with this guide? Have suggestions?
- Open an issue: https://github.com/dvaerum/CoNexus/issues

---

**Written by**: Claude (with testing by the CoNexus team)
**Tested on**: Qwen3-embedding:0.6b, CoNexus (conexus-backend/conexus-router, Rust)

---

## Quick Reference Card

```bash
# Installation
curl -fsSL https://ollama.ai/install.sh | sh

# Pull model
ollama pull qwen3-embedding:0.6b

# Start service
ollama serve

# Configure the environment (no .env file — see "Configuring CoNexus" above)
unset OPENAI_API_KEY
export CONEXUS_LLM_BASE_URL=http://localhost:11434/v1
export CONEXUS_EMBEDDING_MODEL=qwen3-embedding:0.6b
export CONEXUS_EMBEDDING_DIMENSION=1024

# Start the router (see Getting Started for the full command)
conexus-router --port 5454 --projects-file projects.json --sock-dir /tmp/conexus-sockets --dashboard-dir ./result-dashboard/share/conexus-dashboard
```

**That's it!** You're now running CoNexus with free, fast, private local embeddings! 🚀
