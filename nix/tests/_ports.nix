# Shared port assignments for the conexus VM tests.
#
# Held in one place so the multi-tenant + single-tenant scaffolds
# don't drift apart and so the fake-llm sidecar's hardcoded
# 11434 has a documented sibling.
{
  routerPort = 1337;
  fakeOpenAIPort = 11434;
}
