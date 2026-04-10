# ClaudeMonitor - Инструкции для агента

## Технический стек
- Python 3 + PyQt5 (GUI)
- curl_cffi (HTTP для claude.ai API)
- urllib.request (HTTP для Statuspage API)
- Playwright (логин через Chrome)
- PyInstaller (сборка .exe)
- psutil, pywin32

## Структура проекта
```
ClaudeMonitor/
├── usage_monitor.py        # Единственный файл приложения (всё в одном)
├── trayconsole_client.py   # IPC клиент для TrayConsole
├── claude_auth.json        # Сессионные куки (не коммитить)
├── window_state.json       # Позиция окна (не коммитить)
├── extensions/             # Chrome extension для передачи куки
├── ClaudeMonitor.spec      # PyInstaller конфиг
├── PRD.md                  # Требования к продукту
├── planning.md             # Архитектура
├── tasks.md                # Задачи
└── docs/plans/             # Дизайн-документы
```

## Критические правила
1. Всегда читай planning.md и PRD.md перед началом работы.
2. Отмечай выполненные задачи в tasks.md крестиком [x].
3. **ПРАВИЛО ПЕРЕКЛЮЧЕНИЯ РОЛЕЙ:** В tasks.md перед каждой задачей указан тег [Role: Роль]. Переключи контекст на эту роль перед выполнением.
4. Всё приложение - ОДИН файл `usage_monitor.py`. Не создавай дополнительных модулей.
5. Не добавляй новые зависимости без согласования.
6. После изменений - пересобрать через PyInstaller если нужно.

## Использование навыков
- systematic-debugging - при ошибках
- test-driven-development - где применимо
- verification-before-completion - перед завершением задач
