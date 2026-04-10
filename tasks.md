# ClaudeMonitor - Задачи

## Спринт 100: Мониторинг инцидентов Claude Status
**[Role: Desktop App Engineer]**

- [x] {{TASK:100.1}} Добавить IncidentFetchThread для опроса Statuspage API
  **ПОДРОБНОСТИ:**
  - **Что сделать:** Создать класс `IncidentFetchThread(QThread)` в файле `usage_monitor.py`, после существующего класса `PingThread` (после строки 311).
  - **Как сделать:**
    - Сигналы: `result = pyqtSignal(list)`, `error = pyqtSignal(str)`
    - В методе `run()`: использовать `urllib.request.urlopen()` для GET на `https://status.claude.com/api/v2/incidents/unresolved.json` с timeout=10
    - Парсить JSON через `json.loads(response.read().decode())`
    - Из каждого инцидента извлечь: `id`, `name`, `status`, `impact`, `shortlink`
    - Из `incident_updates[0].body` (первый элемент - самое свежее обновление) взять текст последнего обновления. Если массив пуст - пустая строка
    - Вернуть список словарей через `self.result.emit(incidents_list)`
    - Обернуть в try/except, при ошибке `self.error.emit(str(e))`
  - **Ограничения:** Использовать ТОЛЬКО `urllib.request` (стандартная библиотека), НЕ добавлять curl_cffi или requests
  - **Критерии приемки:** Класс создан, при вызове возвращает список инцидентов или пустой список. При недоступности API - сигнал error.

- [x] {{TASK:100.2}} Добавить таймер и обработчики инцидентов в UsageWindow
  **ПОДРОБНОСТИ:**
  - **Что сделать:** В классе `UsageWindow` добавить инициализацию и обработку инцидентов.
  - **Как сделать:**
    - В `__init__`: добавить `self._known_incidents = {}`, `self._first_incident_fetch = True`, `self._toasts = []`, `self._incident_thread = None`
    - В `_init_timers()`: добавить таймер `self._incident_t = QTimer(self)` с интервалом 120000мс (2 минуты), подключить к `self._fetch_incidents`. Сразу вызвать `self._fetch_incidents()` для первого запроса.
    - Метод `_fetch_incidents()`: **ОБЯЗАТЕЛЬНО сохранить ссылку** `self._incident_thread = IncidentFetchThread(self)`, подключить сигналы result -> `_on_incidents`, error -> `lambda _: None` (игнорировать молча). Вызвать `self._incident_thread.start()`.
    - Метод `_on_incidents(incidents_list)`:
      1. Построить `new_ids = {inc["id"] for inc in incidents_list}`
      2. Построить `old_ids = set(self._known_incidents.keys())`
      3. Если НЕ `_first_incident_fetch`:
         - Для каждого id в `new_ids - old_ids`: вызвать `self._show_toast("new", name)`
         - Для каждого id в `old_ids - new_ids`: вызвать `self._show_toast("resolved", old_name)`
      4. Установить `_first_incident_fetch = False`
      5. Обновить `_known_incidents = {inc["id"]: inc for inc in incidents_list}`
      6. Вызвать `_rebuild_incidents(incidents_list)`
    - Метод `_show_toast(kind, name)`: создать `_ToastNotification(kind, name, on_close=self._on_toast_closed)`, добавить в `self._toasts`, вызвать `self._reposition_toasts()`, затем `toast.show()`
    - Метод `_on_toast_closed(toast)`: удалить toast из `self._toasts`, вызвать `self._reposition_toasts()`
    - Метод `_reposition_toasts()`: получить `screen_geo = QApplication.primaryScreen().availableGeometry()`, для каждого toast в `self._toasts` (по индексу i): `toast.move(screen_geo.right() - toast.width() - 16, screen_geo.bottom() - (i + 1) * (toast.height() + 8))`
  - **Ограничения:** Первый fetch НЕ показывает тосты (только заполняет known_incidents). Ошибки fetch инцидентов НЕ показываются в UI (не мешать основному функционалу). ОБЯЗАТЕЛЬНО сохранять ссылку на поток в self._incident_thread (иначе GC убьёт QThread -> segfault).
  - **Критерии приемки:** Таймер запускается, fetch выполняется каждые 2 минуты, при изменении списка инцидентов появляются тосты. Тосты стакаются снизу вверх. При закрытии тоста оставшиеся перестраиваются.

- [x] {{TASK:100.3}} Добавить секцию лейблов инцидентов в UI окна
  **ПОДРОБНОСТИ:**
  - **Что сделать:** Добавить контейнер для лейблов инцидентов в метод `_build()` класса `UsageWindow`, и метод `_rebuild_incidents()`.
  - **Как сделать:**
    - В `_build()`, после `vbox.addWidget(self._body_w)` (строка 494), добавить:
      - `self._incidents_w = QWidget()` - контейнер
      - `self._incidents_layout = QVBoxLayout(self._incidents_w)` с margins(0,4,0,0), spacing(2)
      - `self._incidents_w.hide()` - скрыт по умолчанию
      - `vbox.addWidget(self._incidents_w)`
    - Метод `_rebuild_incidents(incidents)`:
      1. Очистить `_incidents_layout` (удалить все виджеты)
      2. Если incidents пуст - `self._incidents_w.hide()`, return
      3. Для каждого инцидента создать `_IncidentLabel(incident_data)` и добавить в layout
      4. `self._incidents_w.show()`
      5. `self.adjustSize()`
    - При `_enter_compact()` - `self._incidents_w.hide()`
    - При `_exit_compact()` - `if self._known_incidents: self._incidents_w.show()` (показывать ТОЛЬКО если есть инциденты)
  - **Ограничения:** Контейнер не должен занимать место когда инцидентов нет. В compact mode лейблы скрыты. При выходе из compact mode - показывать _incidents_w ТОЛЬКО если _known_incidents не пуст.
  - **Критерии приемки:** При наличии инцидентов внизу окна появляются цветные лейблы. При отсутствии - ничего не видно. Compact mode корректно скрывает/показывает секцию.

- [x] {{TASK:100.4}} Создать виджет _IncidentLabel
  **ПОДРОБНОСТИ:**
  - **Что сделать:** Создать класс `_IncidentLabel(QLabel)` в файле `usage_monitor.py`, рядом с другими виджетами (_ModelRow, _Card).
  - **Как сделать:**
    - Конструктор принимает `incident_data: dict` (id, name, status, impact, shortlink, last_update_body)
    - Сохраняет `self._data = incident_data`
    - Цвета по impact: `_IMPACT_COLORS = {"critical": "#f87171", "major": "#fb923c", "minor": "#facc15", "maintenance": "#60a5fa", "none": "#4ade80"}`
    - Текст: `"● {name}"` обрезанный до 45 символов (с многоточием если длиннее)
    - Стиль: `color:{impact_color};font-size:10px;` + при hover добавить text-decoration:underline через setStyleSheet
    - Курсор: `QCursor(Qt.PointingHandCursor)`
    - Добавить `self._popup = None` в конструктор для отслеживания текущего попапа
    - Переопределить `mousePressEvent`: при левом клике вызвать `self._show_popup()`
    - Переопределить `enterEvent`/`leaveEvent`: добавить/убрать подчёркивание
    - Метод `_show_popup()`: **сначала** закрыть предыдущий попап (`if self._popup: self._popup.close()`), затем создать `self._popup = _IncidentPopup(self._data, self)` и показать рядом с лейблом
  - **Ограничения:** Попап создаётся заново при каждом клике. Предыдущий ОБЯЗАТЕЛЬНО закрывается через self._popup.close() перед созданием нового. Максимум 45 символов в названии.
  - **Критерии приемки:** Лейбл отображает цветную точку + название инцидента, при наведении подчёркивается, при клике открывает попап.

- [x] {{TASK:100.5}} Создать виджет _IncidentPopup
  **ПОДРОБНОСТИ:**
  - **Что сделать:** Создать класс `_IncidentPopup(QWidget)` в `usage_monitor.py`.
  - **Как сделать:**
    - Флаги: `Qt.Popup | Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint` (НЕ добавлять Qt.Tool - конфликтует с Qt.Popup)
    - `setAttribute(Qt.WA_TranslucentBackground)`
    - Реализовать paintEvent прямо в классе (скопировать из _Card: QPainter + QColor(12,12,12,225) + drawRoundedRect). НЕ вкладывать _Card как дочерний виджет - это лишняя сложность для top-level окна.
    - Layout: QVBoxLayout с margins(12, 8, 12, 8)
    - Заголовок: QHBoxLayout с QLabel(name, bold, 11px, white) + QPushButton("x", стиль _SS_BTN, размер 14x14)
    - Статус: QLabel(status, 10px, цвет по impact)
    - Описание: QLabel(last_update_body, 10px, #bbb, wordWrap=True, maxWidth=280)
    - Кнопка Подробнее: QPushButton, стиль как _SS_LOGIN_BTN но синий (#3b82f6), по клику `webbrowser.open(shortlink)`
    - Кнопка X закрывает попап через `self.close()`
    - Позиционирование: рядом с parent label. Вычислить позицию - если label в нижней половине экрана, показать выше, иначе ниже. Смещение по X: выровнять по левому краю label.
    - `setFixedWidth(300)`
  - **Ограничения:** Попап НЕ модальный. Закрывается автоматически при потере фокуса (Qt.Popup делает это). Ширина фиксирована 300px, высота - auto.
  - **Критерии приемки:** Попап появляется рядом с лейблом, показывает название + статус + описание + кнопку Подробнее. Закрывается по клику вне или по кнопке X. Кнопка Подробнее открывает браузер.

- [x] {{TASK:100.6}} Создать виджет _ToastNotification
  **ПОДРОБНОСТИ:**
  - **Что сделать:** Создать класс `_ToastNotification(QWidget)` в `usage_monitor.py`.
  - **Как сделать:**
    - Флаги: `Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool`
    - `setAttribute(Qt.WA_TranslucentBackground)`
    - `setAttribute(Qt.WA_ShowWithoutActivating)` - не забирать фокус
    - Реализовать paintEvent прямо в классе (скопировать из _Card: QPainter + QColor(12,12,12,225) + drawRoundedRect). НЕ вкладывать _Card как дочерний виджет.
    - Layout: QHBoxLayout с margins(12, 8, 12, 8)
    - Иконка: QLabel с цветным символом "!" (цвет #f87171) для нового инцидента, "OK" (цвет #4ade80) для завершённого. Шрифт bold 11px.
    - Текст: QLabel(message, 10px, #e0e0e0, wordWrap=True)
    - Кнопка X: QPushButton("x", стиль _SS_BTN, 14x14)
    - `setFixedWidth(320)`
    - Конструктор принимает `kind: str` ("new"/"resolved"), `name: str`, `on_close: callable`
    - Кнопка X вызывает `self._on_close(self)` затем `self.close()` и `self.deleteLater()`
    - Позиционирование: управляется ИЗВНЕ через `UsageWindow._reposition_toasts()`, использующий `QApplication.primaryScreen().availableGeometry()` (НЕ deprecated `QApplication.desktop()`). Отступ 16px от правого и нижнего края экрана. Стакинг снизу вверх по индексу в списке `_toasts`.
  - **Ограничения:** Без анимации. Без звука. Остаётся до закрытия пользователем. Не забирает фокус при появлении.
  - **Критерии приемки:** Toast появляется в правом нижнем углу, не забирает фокус, стакается при нескольких инцидентах, закрывается по X, оставшиеся тосты перестраиваются.

- [x] {{TASK:100.7}} Интеграция компонентов и контекстное меню
  **ПОДРОБНОСТИ:**
  - **Что сделать:** Финальная интеграция всех компонентов в `usage_monitor.py`.
  - **Как сделать:**
    - Добавить `import webbrowser` и `import urllib.request` в блок импортов в начале файла (после `from datetime import ...`)
    - В `_enter_compact()` (строка 514): добавить `self._incidents_w.hide()`
    - В `_exit_compact()` (строка 524): добавить `if self._known_incidents: self._incidents_w.show()`
    - В `_setup_trayconsole()`: в обработчике `custom:refresh` добавить вызов `win._fetch_incidents()` после `win._fetch`
    - В `contextMenuEvent()` (строка 745): добавить пункт "Claude Status" между "Обновить" и "Свернуть в трей", по клику `webbrowser.open("https://status.claude.com")`
  - **Ограничения:** Не менять существующую функциональность. Не добавлять новые зависимости.
  - **Критерии приемки:** Compact mode корректно скрывает/показывает инциденты. TrayConsole refresh обновляет и инциденты. Контекстное меню содержит пункт Claude Status, открывающий браузер.
