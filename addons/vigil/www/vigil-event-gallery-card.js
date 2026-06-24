/**
 * vigil-event-gallery-card
 *
 * Lovelace custom card that renders recent Vigil detection events from HA
 * state entities and lets the operator publish correction commands back to
 * Vigil via HA's MQTT publish service.
 *
 * Add to a Lovelace dashboard:
 *   type: custom:vigil-event-gallery-card
 */

class VigilEventGalleryCard extends HTMLElement {
    constructor() {
        super();
        this._hass = null;
        this._config = {};
        this.attachShadow({ mode: 'open' });
    }

    setConfig(config) {
        this._config = config || {};
    }

    set hass(hass) {
        this._hass = hass;
        this._render();
    }

    _render() {
        if (!this._hass) return;

        // Collect Vigil event entities — they are registered with event_types
        // containing "vigil_detection" via MQTT discovery.
        const events = Object.values(this._hass.states)
            .filter(s =>
                s.entity_id.startsWith('event.') &&
                s.attributes &&
                Array.isArray(s.attributes.event_types) &&
                s.attributes.event_types.includes('vigil_detection')
            )
            .sort((a, b) => (b.last_changed || '').localeCompare(a.last_changed || ''))
            .slice(0, this._config.max_events || 20);

        this.shadowRoot.innerHTML = `
            <style>
                :host {
                    display: block;
                    font-family: var(--primary-font-family, sans-serif);
                }
                .card-header {
                    padding: 16px 16px 8px;
                    font-size: 1.1em;
                    font-weight: 500;
                    color: var(--primary-text-color);
                }
                .card-content {
                    padding: 0 16px 16px;
                }
                .empty {
                    color: var(--secondary-text-color);
                    font-size: 0.9em;
                    padding: 8px 0;
                }
                .event-row {
                    border: 1px solid var(--divider-color, #e0e0e0);
                    border-radius: 6px;
                    margin: 8px 0;
                    padding: 10px 12px;
                    background: var(--card-background-color, #fff);
                }
                .event-title {
                    font-weight: 500;
                    font-size: 0.95em;
                    color: var(--primary-text-color);
                }
                .event-meta {
                    font-size: 0.78em;
                    color: var(--secondary-text-color);
                    margin-top: 2px;
                }
                .btn-row {
                    margin-top: 8px;
                    display: flex;
                    gap: 6px;
                    flex-wrap: wrap;
                }
                .btn {
                    padding: 4px 10px;
                    border: none;
                    border-radius: 4px;
                    font-size: 0.82em;
                    cursor: pointer;
                    font-family: inherit;
                }
                .btn:hover { opacity: 0.85; }
                .btn-identity  { background: #4CAF50; color: #fff; }
                .btn-wrong     { background: #FF9800; color: #fff; }
                .btn-false     { background: #F44336; color: #fff; }
                .sent-badge {
                    font-size: 0.75em;
                    color: var(--success-color, #4CAF50);
                    margin-left: 6px;
                }
                .wrong-class-form {
                    display: inline-flex;
                    gap: 4px;
                    align-items: center;
                    margin-left: 4px;
                }
                .wrong-class-input {
                    padding: 3px 6px;
                    font-size: 0.82em;
                    border: 1px solid var(--divider-color, #ccc);
                    border-radius: 4px;
                    font-family: inherit;
                    width: 120px;
                }
                .btn-send-wrong {
                    padding: 3px 8px;
                    background: #FF9800;
                    color: #fff;
                    border: none;
                    border-radius: 4px;
                    font-size: 0.82em;
                    cursor: pointer;
                    font-family: inherit;
                }
                .btn-send-wrong:hover { opacity: 0.85; }
            </style>
            <div class="card-header">Vigil Detections</div>
            <div class="card-content">
                ${events.length === 0
                    ? '<div class="empty">No Vigil detection events found. ' +
                      'Ensure Vigil is running and MQTT discovery is active.</div>'
                    : events.map(e => this._renderEventRow(e)).join('')}
            </div>
        `;

        // Wire up correction buttons.
        this.shadowRoot.querySelectorAll('.btn[data-detection-id]').forEach(btn => {
            btn.addEventListener('click', () => {
                const { detectionId, correctionType } = btn.dataset;
                if (correctionType === 'wrong_class') {
                    this._showWrongClassInput(detectionId, btn);
                } else {
                    const label = btn.dataset.label || null;
                    this._sendCorrection(detectionId, correctionType, label, btn);
                }
            });
        });
    }

    _renderEventRow(entity) {
        const attrs = entity.attributes || {};
        // detection_id is carried in the event_data attribute when Vigil fires
        // the vigil_detection event; fall back to entity_id slug.
        const rawState = entity.state || '';
        let detectionId = '';
        try {
            // HA event entities store last event data in 'state' as JSON or in attrs
            const evData = attrs.event_data || (rawState.startsWith('{') ? JSON.parse(rawState) : {});
            detectionId = evData.detection_id || attrs.detection_id || entity.entity_id;
        } catch {
            detectionId = attrs.detection_id || entity.entity_id;
        }

        const camera   = attrs.friendly_name || entity.entity_id.replace('event.', '').replace(/_detection$/, '');
        const cls      = attrs.object_class  || (attrs.event_data && attrs.event_data.object_class) || '?';
        const conf     = attrs.confidence    != null ? (Number(attrs.confidence) * 100).toFixed(0) + '%' : '';
        const time     = entity.last_changed
            ? new Date(entity.last_changed).toLocaleString(undefined, { dateStyle: 'short', timeStyle: 'medium' })
            : '';

        // Escape for HTML attribute safety (detection_id is a UUID, but be safe)
        const safeId = (detectionId + '').replace(/"/g, '');

        return `
            <div class="event-row">
                <div class="event-title">${_esc(camera)} &mdash; ${_esc(cls)} ${_esc(conf)}</div>
                <div class="event-meta">${_esc(time)}</div>
                <div class="btn-row">
                    <button class="btn btn-identity"
                        data-detection-id="${safeId}"
                        data-correction-type="identity"
                        data-label="confirmed">
                        &#10003; Identity
                    </button>
                    <button class="btn btn-wrong"
                        data-detection-id="${safeId}"
                        data-correction-type="wrong_class"
                        title="Enter the corrected class label">
                        &#9888; Wrong class&hellip;
                    </button>
                    <button class="btn btn-false"
                        data-detection-id="${safeId}"
                        data-correction-type="false_alarm">
                        &#10007; False alarm
                    </button>
                </div>
            </div>
        `;
    }

    _showWrongClassInput(detectionId, triggerBtn) {
        // If an input form is already open for this button, just focus it.
        const existing = triggerBtn.parentElement &&
            triggerBtn.parentElement.querySelector('.wrong-class-form');
        if (existing) {
            existing.querySelector('input').focus();
            return;
        }

        const form = document.createElement('span');
        form.className = 'wrong-class-form';
        form.innerHTML =
            '<input type="text" class="wrong-class-input" placeholder="corrected class…" />' +
            '<button class="btn-send-wrong">Send</button>';
        triggerBtn.after(form);

        const input = form.querySelector('input');
        const sendBtn = form.querySelector('.btn-send-wrong');
        input.focus();

        const submit = () => {
            const label = input.value.trim();
            if (!label) return;
            this._sendCorrection(detectionId, 'wrong_class', label, sendBtn);
            form.remove();
        };

        input.addEventListener('keydown', e => {
            if (e.key === 'Enter') submit();
            if (e.key === 'Escape') form.remove();
        });
        sendBtn.addEventListener('click', submit);
    }

    _sendCorrection(detectionId, correctionType, label, btn) {
        if (!this._hass || !detectionId) return;
        const payload = JSON.stringify({
            detection_id:    detectionId,
            correction_type: correctionType,
            ...(label != null ? { label } : {}),
        });
        this._hass.callService('mqtt', 'publish', {
            topic:   'vigil/commands/correct',
            payload,
        }).then(() => {
            const badge = document.createElement('span');
            badge.className = 'sent-badge';
            badge.textContent = 'sent';
            btn.after(badge);
            setTimeout(() => badge.remove(), 3000);
        }).catch(err => {
            console.error('[vigil-event-gallery-card] correction publish failed:', err);
        });
    }

    getCardSize() {
        return 4;
    }

    static getConfigElement() {
        // No visual config editor — card works with an empty config.
        return document.createElement('div');
    }

    static getStubConfig() {
        return { max_events: 20 };
    }
}

function _esc(str) {
    return String(str || '')
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
}

customElements.define('vigil-event-gallery-card', VigilEventGalleryCard);

window.customCards = window.customCards || [];
window.customCards.push({
    type:        'vigil-event-gallery-card',
    name:        'Vigil Event Gallery',
    description: 'Shows recent Vigil detection events with inline correction buttons.',
});
