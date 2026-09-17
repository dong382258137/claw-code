// 对照实验：用 node 运行官方 DeepSeekHashV1 wasm，复刻 openclaw 的调用，验证正确 answer。
// 用法: node pow-ref.js
const fs = require('fs');
const b64 = fs.readFileSync('d:\\claw-code-src\\rust\\crates\\web-bridge\\src\\deepseek_hash_v1.wasm.b64.txt', 'utf8');
const bytes = Buffer.from(b64, 'base64');

const C = '939af97ca1e864791161d4aa4acd22fd45622c7f18395261d4ec976a19469d02';
const SALT = '281797e02c14c558f2cb';
const EXPIRE = 1789643042517;
const DIFF = 144000;

WebAssembly.instantiate(bytes, { wbg: {} }).then(({ instance }) => {
  const ex = instance.exports;
  console.log('exports:', Object.keys(ex));
  const alloc = ex.__wbindgen_export_0;
  const stack_ptr = ex.__wbindgen_add_to_stack_pointer;
  const wasm_solve = ex.wasm_solve;
  const memory = ex.memory;

  const encode = (str) => {
    const buf = Buffer.from(str, 'utf8');
    const ptr = alloc(buf.length, 1);
    new Uint8Array(memory.buffer).set(buf, ptr);
    return [ptr, buf.length];
  };
  const prefix = `${SALT}_${EXPIRE}_`;
  const [pC, lC] = encode(C);
  const [pP, lP] = encode(prefix);
  const retptr = stack_ptr(-16);
  const t0 = Date.now();
  wasm_solve(retptr, pC, lC, pP, lP, DIFF);
  console.log(`solved in ${Date.now() - t0}ms`);
  const view = new DataView(memory.buffer);
  const status = view.getInt32(retptr, true);
  const answer = view.getFloat64(retptr + 8, true);
  stack_ptr(16);
  console.log('status =', status, ' answer =', answer, ' answerHex =', Buffer.from(new Uint8Array(memory.buffer, retptr + 8, 8).slice(0)).toString('hex'));
  // 也看看 && 签名
  process.exit(0);
}).catch((e) => { console.error('FATAL', e); process.exit(1); });