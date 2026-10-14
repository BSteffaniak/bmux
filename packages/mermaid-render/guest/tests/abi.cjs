// Run: node packages/mermaid-render/guest/tests/abi.cjs /path/to/mermaid.wasm
// No npm dependencies; this executes the real compiled guest, not an ABI mock.
const assert = require('node:assert/strict');
const fs = require('node:fs');

const bytes = fs.readFileSync(process.argv[2]);
const module_ = new WebAssembly.Module(bytes);
assert.deepEqual(WebAssembly.Module.imports(module_), [], 'no runtime imports');
const cases = [
  [Buffer.from('flowchart LR\n A[Hello] --> B[World]'), 0],
  [Buffer.from('sequenceDiagram\n Alice->>Bob: Hello'), 0],
  [Buffer.from('classDiagram\n Animal <|-- Duck'), 0],
  [Buffer.from('stateDiagram-v2\n [*] --> Ready\n Ready --> [*]'), 0],
  [Buffer.from('pie title Pets\n "Dogs" : 60\n "Cats" : 40'), 0],
  [Buffer.from('erDiagram\n CUSTOMER ||--o{ ORDER : places'), 0],
  [Buffer.from('flowchart LR\n A[日本語] --> B[Hello 🌍]'), 0],
  [Buffer.from('not a mermaid diagram'), 1],
  [Buffer.from([255, 254]), 1],
];
const results = [];
for (let instanceIndex = 0; instanceIndex < 2; instanceIndex++) {
  const { memory, alloc, dealloc, render } = new WebAssembly.Instance(module_).exports;
  for (let repetition = 0; repetition < 3; repetition++) {
    for (let caseIndex = 0; caseIndex < cases.length; caseIndex++) {
      const [input, status] = cases[caseIndex];
      const ptr = alloc(input.length);
      new Uint8Array(memory.buffer, ptr, input.length).set(input);
      const packed = render(ptr, input.length);
      const outputPtr = Number(packed >> 32n);
      const outputLen = Number(packed & 0xffffffffn);
      assert.notEqual(ptr, outputPtr, 'output must not alias live input');
      assert.deepEqual(Buffer.from(new Uint8Array(memory.buffer, ptr, input.length)), input);
      dealloc(ptr, input.length);
      const output = Buffer.from(new Uint8Array(memory.buffer, outputPtr, outputLen));
      assert.equal(output[0], status, `case ${caseIndex}`);
      const text = new TextDecoder('utf-8', { fatal: true }).decode(output.subarray(1));
      assert(text.length > 0);
      if (status === 0) {
        assert(text.includes('<svg'));
        assert(text.includes('</svg>'));
      }
      if (instanceIndex === 0 && repetition === 0) results.push(output);
      else assert.deepEqual(output, results[caseIndex], 'deterministic across instances and reuse');
      dealloc(outputPtr, outputLen);
    }
  }
}
console.log(`PASS: ${cases.length * 6} real guest renders; no imports; ownership, UTF-8, deterministic output; ${bytes.length} WASM bytes`);
