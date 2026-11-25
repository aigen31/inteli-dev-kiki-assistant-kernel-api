// Deterministic OpenAI-compatible mock for local kernel checks.
//
// It exercises the real tool loop, the CRM provider, confirmations and the
// image path without calling a paid model:
//
//   * a message mentioning "заявк"/"форм" proposes create_task_from_form;
//   * a message with an image answers as if it saw the picture;
//   * anything else calls the `overview` tool and then answers;
//   * after a tool result the final answer is plain text.
const http = require('http');

const FORM_CALL = {
  id: 'call_mock_form',
  type: 'function',
  function: {
    name: 'create_task_from_form',
    arguments: JSON.stringify({
      form_key: 'bug-report',
      title: 'Кнопка входа не работает',
      priority: 'high',
      form_data: {
        fields: {
          severity: 'high',
          what_happened: 'Кнопка «Войти» не реагирует на нажатие.',
          expected: 'Открывается личный кабинет.',
          actual: 'Страница остаётся на месте.',
          environment: 'Chrome, desktop',
        },
        groups: { steps: [] },
      },
    }),
  },
};

function completion(body, message) {
  return {
    id: 'mock-completion',
    object: 'chat.completion',
    model: body.model || 'mock',
    choices: [{ index: 0, message, finish_reason: message.tool_calls ? 'tool_calls' : 'stop' }],
    usage: { prompt_tokens: 60, completion_tokens: 18, total_tokens: 78 },
  };
}

function lastUserText(messages) {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (messages[i].role !== 'user') continue;
    const content = messages[i].content;
    if (typeof content === 'string') return { text: content, parts: 0 };
    if (Array.isArray(content)) {
      return {
        text: content.filter((p) => p.type === 'text').map((p) => p.text).join(' '),
        parts: content.filter((p) => p.type === 'image_url').length,
      };
    }
  }
  return { text: '', parts: 0 };
}

const server = http.createServer((req, res) => {
  let raw = '';
  req.on('data', (chunk) => (raw += chunk));
  req.on('end', () => {
    let body = {};
    try {
      body = JSON.parse(raw || '{}');
    } catch (_) {
      body = {};
    }

    const messages = body.messages || [];
    const sawTool = messages.some((m) => m.role === 'tool');
    const last = lastUserText(messages);

    let payload;

    if (sawTool) {
      payload = completion(body, {
        role: 'assistant',
        content: 'Проверила CRM: данные получены. В работе есть задачи, подробности в интерфейсе.',
      });
    } else if (last.parts > 0) {
      payload = completion(body, {
        role: 'assistant',
        content: `Вижу изображение (${last.parts}). Похоже на скриншот интерфейса — могу оформить заявку, если нужно.`,
      });
    } else if (/заявк|форм/i.test(last.text)) {
      payload = completion(body, { role: 'assistant', content: '', tool_calls: [FORM_CALL] });
    } else {
      payload = completion(body, {
        role: 'assistant',
        content: '',
        tool_calls: [
          {
            id: 'call_mock_overview',
            type: 'function',
            function: { name: 'overview', arguments: '{}' },
          },
        ],
      });
    }

    const data = Buffer.from(JSON.stringify(payload));
    res.writeHead(200, { 'content-type': 'application/json', 'content-length': data.length });
    res.end(data);
  });
});

server.listen(18091, '0.0.0.0');
