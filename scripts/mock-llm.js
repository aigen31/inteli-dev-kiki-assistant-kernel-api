const http = require('http');
const server = http.createServer((req, res) => {
  let raw = '';
  req.on('data', c => raw += c);
  req.on('end', () => {
    let body = {}; try { body = JSON.parse(raw || '{}'); } catch (_) {}
    const messages = body.messages || [];
    const hasTool = messages.some(m => m.role === 'tool');
    const payload = hasTool ? {
      id: 'mock-final', object: 'chat.completion', model: body.model || 'mock',
      choices: [{index:0, message:{role:'assistant', content:'Проверила CRM: данные получены. В работе есть задачи, подробности в интерфейсе.'}, finish_reason:'stop'}],
      usage: {prompt_tokens:80, completion_tokens:18, total_tokens:98}
    } : {
      id: 'mock-tool', object: 'chat.completion', model: body.model || 'mock',
      choices: [{index:0, message:{role:'assistant', content:'', tool_calls:[{id:'call_mock_overview',type:'function',function:{name:'overview',arguments:'{}'}}]}, finish_reason:'tool_calls'}],
      usage: {prompt_tokens:60, completion_tokens:12, total_tokens:72}
    };
    const data = Buffer.from(JSON.stringify(payload));
    res.writeHead(200, {'content-type':'application/json','content-length':data.length});
    res.end(data);
  });
});
server.listen(18091, '0.0.0.0');
