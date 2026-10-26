import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createServer } from 'node:http';
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import { tmpdir } from 'node:os';

// Actual compiled Rust/WASM + Canvas + browser input, not a mock application.
// Requires an exact matching bindgen CLI; no implicit installs or PATH fallback.
test('standalone compiled custom component browser acceptance', {
  skip: process.env.BMUX_CUSTOM_BROWSER_TEST !== '1' ? 'set BMUX_CUSTOM_BROWSER_TEST=1 and CHROMIUM_BIN' : false,
  timeout: 180000,
}, async () => {
  assert.ok(process.env.CHROMIUM_BIN, 'explicit Chromium executable required');
  const build = spawnSync('nix', ['develop', '--command', 'bash', 'packages/web-renderer/tests/build-custom-component.sh'],
    { encoding:'utf8', timeout:120000 });
  assert.equal(build.status, 0, build.stdout + build.stderr);
  const root = resolve('packages/web-renderer');
  const server = createServer(async (req, res) => {
    try {
      const path = new URL(req.url, 'http://localhost').pathname;
      const file = path.startsWith('/examples/pkg/') ? resolve('target/custom-component-web/pkg', path.slice(14)) : resolve(root, '.' + path);
      assert.ok(file.startsWith(root + '/') || file.startsWith(resolve('target/custom-component-web/pkg') + '/'));
      let data = await readFile(file);
      if (path === '/examples/custom-component.html') {
        // Test-only access to example-owned controls; no production debug API.
        data = Buffer.from(data.toString().replace('</script>',
          'globalThis.acceptance = {roots, embeds, mount, status};</script>'));
      }
      res.setHeader('Content-Type', ({'.html':'text/html','.js':'text/javascript','.wasm':'application/wasm'})[extname(file)] ?? 'text/plain');
      res.end(data);
    } catch (error) { res.statusCode = 404; res.end(String(error)); }
  });
  await new Promise(done => server.listen(0, '127.0.0.1', done));
  const profile = await mkdtemp(resolve(tmpdir(), 'bmux-custom-browser-'));
  const child = spawn(process.env.CHROMIUM_BIN, ['--headless','--disable-gpu','--no-first-run',
    '--no-default-browser-check','--remote-debugging-pipe','--user-data-dir='+profile],
    {stdio:['ignore','ignore','pipe','pipe','pipe']});
  let stderr = '', buffer = '', sequence = 0;
  const pending = new Map();
  child.stderr.on('data', data => { stderr = (stderr+data).slice(-4000); });
  child.stdio[4].on('data', data => {
    buffer += data.toString();
    for (let end; (end = buffer.indexOf('\0')) >= 0;) {
      const message = JSON.parse(buffer.slice(0,end)); buffer = buffer.slice(end+1);
      const entry = pending.get(message.id);
      if (entry) { pending.delete(message.id); clearTimeout(entry.timer); message.error ? entry.reject(Error(JSON.stringify(message.error))) : entry.resolve(message.result); }
    }
  });
  const exited = new Promise(done => child.once('exit', done));
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(Error(method+' timed out: '+stderr)); }, 15000);
    pending.set(id, {resolve,reject,timer});
    child.stdio[3].write(JSON.stringify({id,method,params,...(sessionId ? {sessionId} : {})})+'\0');
  });
  try {
    const {targetId} = await send('Target.createTarget', {url:'about:blank'});
    const {sessionId} = await send('Target.attachToTarget', {targetId,flatten:true});
    const page = (method, params) => send(method,params,sessionId);
    const evaluate = async expression => {
      const result = await page('Runtime.evaluate', {expression,awaitPromise:true,returnByValue:true});
      assert.ok(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
      return result.result.value;
    };
    const wait = async expression => {
      for (let i=0;i<150;i++) {
        if (await evaluate(expression)) return;
        await new Promise(done => setTimeout(done,30));
      }
      assert.fail('browser condition timed out: '+expression+'; '+await evaluate('document.body.innerText'));
    };
    await page('Page.navigate', {url:`http://127.0.0.1:${server.address().port}/examples/custom-component.html`});
    await wait('!!globalThis.acceptance && acceptance.roots.every(r=>r.querySelector("[data-semantic-id=editor]"))');
    const label = i => `acceptance.roots[${i}].querySelector('[data-semantic-id="counter:increment"]')?.getAttribute('aria-label')`;
    const text = i => `acceptance.roots[${i}].querySelector('[data-semantic-id=editor]').textContent`;
    const point = async (i,x,y) => evaluate(`(()=>{const r=acceptance.roots[${i}].querySelector('canvas').getBoundingClientRect();return {x:r.left+${x},y:r.top+${y}}})()`);
    const mouse = async (type,i,x,y,buttons=0) => page('Input.dispatchMouseEvent', {type,...await point(i,x,y),button:buttons||type==='mouseReleased'?'left':'none',buttons,clickCount:type==='mouseMoved'?0:1});
    const click = async (i,x,y) => { await mouse('mousePressed',i,x,y,1); await mouse('mouseReleased',i,x,y); };
    await click(0,15,10);
    await wait(`${label(0)} === 'Increment: 1'`);
    assert.equal(await evaluate(label(1)), 'Increment: 0');
    await click(1,15,50);
    await page('Input.insertText', {text:'界!'});
    await wait(`${text(1)}.endsWith('界!')`);
    await page('Input.dispatchKeyEvent', {type:'keyDown',key:'Backspace',code:'Backspace',windowsVirtualKeyCode:8});
    await page('Input.dispatchKeyEvent', {type:'keyUp',key:'Backspace',code:'Backspace',windowsVirtualKeyCode:8});
    await wait(`${text(1)}.endsWith('界')`);
    assert.equal(await evaluate(text(0)), 'Drag to select this caller-owned source text.');
    assert.equal(await evaluate(`acceptance.roots[0].querySelector('canvas').getContext('2d').getImageData(0,0,1,1).data[3]`),255);

    await evaluate(`globalThis.copies=[]; acceptance.embeds[0].dispose(); acceptance.embeds[0]=acceptance.mount(acceptance.roots[0],{writeClipboard:async text=>copies.push(text)});`);
    await wait(`${label(0)} === 'Increment: 0'`);
    await mouse('mousePressed',0,1,50,1);
    await mouse('mouseMoved',0,49,50,1);
    await mouse('mouseReleased',0,49,50);
    await wait(`!acceptance.status.textContent`);
    // copy() is invoked only when the actual WASM presentation has settled.
    await evaluate(`(async()=>{for(let i=0;i<100;i++){try{await acceptance.embeds[0].copy();return}catch(e){if(!String(e).includes('busy'))throw e;await new Promise(r=>setTimeout(r,10))}}throw Error('copy remained busy')})()`);
    assert.equal(await evaluate('copies[0]'), 'Drag ');
    await evaluate(`acceptance.roots[0].style.width='0px';acceptance.roots[0].style.height='0px'`);
    await wait(`acceptance.roots[0].querySelector('canvas').width===0 && acceptance.roots[0].querySelector('canvas').height===0`);
    await evaluate(`acceptance.roots[0].style.width='1px';acceptance.roots[0].style.height='1px'`);
    await new Promise(done=>setTimeout(done,100));
    assert.equal(await evaluate('acceptance.status.textContent'),'');
    await evaluate(`acceptance.roots[0].style.width='540px';acceptance.roots[0].style.height='160px'`);
    await wait(`${label(0)} === 'Increment: 0'`);
    await page('Emulation.setDeviceMetricsOverride', {width:1000,height:900,deviceScaleFactor:2,mobile:false});
    await wait(`acceptance.roots[0].querySelector('canvas').width===1080`);
    await click(0,15,10); await wait(`${label(0)} === 'Increment: 1'`);
    assert.ok((await evaluate(text(1))).endsWith('界'));
    await evaluate(`acceptance.embeds[0].dispose()`);
    assert.equal(await evaluate(`acceptance.roots[0].querySelectorAll('textarea,[data-bmux-semantics]').length`),0);
    await click(0,15,10);
    await evaluate(`acceptance.embeds[0]=acceptance.mount(acceptance.roots[0],{writeClipboard:async()=>{throw Error('capability denied')}})`);
    await wait(`${label(0)} === 'Increment: 0'`);
    assert.equal(await evaluate(`acceptance.embeds[0].copy().then(()=>false,e=>String(e).includes('capability denied'))`),true);
    assert.equal(await evaluate(`acceptance.roots[0].querySelectorAll('textarea').length`),1);
    assert.equal(await evaluate('acceptance.status.textContent'),'');
    await evaluate('acceptance.embeds.forEach(e=>e.dispose())');
  } finally {
    child.kill(); await exited;
    for (const entry of pending.values()) { clearTimeout(entry.timer); entry.reject(Error('browser closed')); }
    await new Promise(done => server.close(done));
    await rm(profile,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  }
});
