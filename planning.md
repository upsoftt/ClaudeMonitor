# ClaudeMonitor - Архитектура

## Общая архитектура
Однофайловое PyQt5-приложение (`usage_monitor.py`) с многопоточной моделью. GUI в главном потоке, фоновые операции (логин, fetch, ping, инциденты) в QThread.

## Компоненты фичи мониторинга инцидентов

### 1. IncidentFetchThread (QThread)
- Сигналы: `result(list)`, `error(str)`
- Делает GET на `https://status.claude.com/api/v2/incidents/unresolved.json` через `urllib.request`
- Парсит JSON, возвращает список словарей: `{id, name, status, impact, shortlink, last_update_body}`
- Timeout: 10 секунд
- User-Agent: стандартный Python

### 2. Секция инцидентов в UsageWindow
- Контейнер `_incidents_w` (QWidget) внизу `_body_w`, под строками моделей
- Содержит `_incidents_layout` (QVBoxLayout) с лейблами `_IncidentLabel`
- Скрывается если инцидентов нет (не занимает место)

### 3. _IncidentLabel (QLabel)
- Кликабельный лейбл с названием инцидента
- Цвет по impact: critical=#f87171, major=#fb923c, minor=#facc15, maintenance=#60a5fa
- Курсор pointer, подчёркивание при hover
- По клику создаёт/показывает `_IncidentPopup`

### 4. _IncidentPopup (QWidget)
- Qt.Popup | Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint (НЕ Qt.Tool — конфликтует с Popup)
- WA_TranslucentBackground, собственный paintEvent (как в _Card: QColor(12,12,12,225) + drawRoundedRect)
- Содержит: название, статус, текст последнего обновления, кнопка Подробнее (webbrowser.open shortlink), кнопка X
- Позиционируется рядом с лейблом (выше, если лейбл внизу экрана)
- Закрывается по клику X или по клику вне попапа (Qt.Popup)
- Ширина 300px, высота auto

### 5. _ToastNotification (QWidget)
- Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool
- WA_TranslucentBackground, WA_ShowWithoutActivating, собственный paintEvent (как в _Card)
- Правый нижний угол экрана (отступ 16px), позиционирование через UsageWindow._reposition_toasts()
- Текст: иконка ("!" красный / "OK" зелёный) + сообщение + кнопка X
- Конструктор принимает kind, name, on_close callback
- Остаётся до закрытия пользователем, не забирает фокус
- Множественные тосты стакаются вверх (каждый следующий выше предыдущего)

### 6. Отслеживание состояния инцидентов
- `_known_incidents: dict` в UsageWindow - ключ id, значение dict инцидента
- При каждом fetch сравниваем новые id с известными:
  - Новый id -> toast "Инцидент: {name}"
  - Исчезнувший id -> toast "Завершён: {name}"
- Первый fetch после старта не генерирует тосты (только заполняет _known_incidents)

## Потоки данных

```
Timer (120s) -> IncidentFetchThread -> result signal -> UsageWindow._on_incidents()
                                                         |
                                                         +-> обновить лейблы
                                                         +-> сравнить с _known_incidents
                                                         +-> показать тосты для новых/завершённых
                                                         +-> обновить _known_incidents
```

## API эндпоинт (верифицирован)

**URL:** `https://status.claude.com/api/v2/incidents/unresolved.json`
**Метод:** GET
**Аутентификация:** не требуется
**Rate limit:** нет (публичный Statuspage)

**Структура ответа:**
```json
{
  "page": {"id": "...", "name": "Claude", "url": "https://status.claude.com"},
  "incidents": [
    {
      "id": "d8r794mwjg8d",
      "name": "Elevated connection reset errors in Cowork",
      "status": "investigating",
      "impact": "minor",
      "shortlink": "https://stspg.io/5zt95qbyf06z",
      "created_at": "2026-03-25T14:33:53.402Z",
      "incident_updates": [
        {
          "id": "...",
          "status": "investigating",
          "body": "We are continuing to investigate...",
          "created_at": "2026-03-25T16:56:52.571Z"
        }
      ]
    }
  ]
}
```
