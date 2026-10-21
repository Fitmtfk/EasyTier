import { mount, type VueWrapper } from '@vue/test-utils'
import { describe, expect, it, vi } from 'vitest'
import { nextTick, reactive } from 'vue'
import PrimeVue from 'primevue/config'
import Config from '../src/components/Config.vue'
import { NetworkConfig as NetworkConfigPb } from '../src/generated/proto/api_manage'
import cn from '../src/locales/cn.yaml'
import en from '../src/locales/en.yaml'
import {
  DEFAULT_NETWORK_CONFIG,
  toBackendNetworkConfig,
  type NetworkConfig,
} from '../src/types/network'

vi.mock('vue-i18n', () => ({
  useI18n: () => ({
    t: (key: string, values?: unknown[]) => values ? `${key}:${values.join(',')}` : key,
  }),
}))

const FIELD = 'direct_conn_family'

/** The three states the selector offers, in the order the schema states them. */
const OPTIONS = ['auto', 'prefer_ipv4', 'prefer_ipv6'] as const

function mountConfig(config: NetworkConfig = DEFAULT_NETWORK_CONFIG()) {
  const curNetwork = reactive(config) as NetworkConfig
  const wrapper = mount(Config, {
    props: { curNetwork },
    global: {
      plugins: [PrimeVue],
      directives: { tooltip: () => {} },
      stubs: { AclManager: true, UrlListInput: true },
    },
  })

  return { curNetwork, wrapper }
}

/** The option buttons the selector renders, in order. */
function options(wrapper: VueWrapper): VueWrapper[] {
  return wrapper.findAll(`#${FIELD} button`)
}

/** The option the selector shows as chosen. */
function selected(wrapper: VueWrapper): string | undefined {
  return options(wrapper)
    .filter((option) => option.attributes('aria-pressed') === 'true')
    .map((option) => option.text())
    .at(0)
}

describe('direct connection family selector', () => {
  it('the management API carries the new field', () => {
    expect(NetworkConfigPb.fields.some((f) => f.name === FIELD)).toBe(true)
  })

  it('offers every state and falls back to auto when the config states none', async () => {
    const { curNetwork, wrapper } = mountConfig()
    await nextTick()

    expect(options(wrapper)).toHaveLength(3)
    expect(options(wrapper).map((option) => option.text())).toEqual([...OPTIONS])
    expect(selected(wrapper)).toBe('auto')
    expect(wrapper.find(`#${FIELD}`).text()).toContain('auto')

    // A config that states nothing keeps stating nothing: the backend
    // resolves the default itself.
    expect(curNetwork.direct_conn_family).toBeUndefined()
    expect(toBackendNetworkConfig(curNetwork).direct_conn_family).toBeUndefined()
  })

  it('shows the state the config states', async () => {
    const config = DEFAULT_NETWORK_CONFIG()
    config.direct_conn_family = 'prefer_ipv4'
    const { wrapper } = mountConfig(config)
    await nextTick()

    expect(selected(wrapper)).toBe('prefer_ipv4')
  })

  it('falls back to auto for an empty or unknown state', async () => {
    const empty = DEFAULT_NETWORK_CONFIG()
    empty.direct_conn_family = ''
    const emptyWrapper = mountConfig(empty)
    await nextTick()
    expect(selected(emptyWrapper.wrapper)).toBe('auto')

    const unknown = DEFAULT_NETWORK_CONFIG()
    unknown.direct_conn_family = 'ipv4'
    const unknownWrapper = mountConfig(unknown)
    await nextTick()
    expect(selected(unknownWrapper.wrapper)).toBe('auto')
  })

  it('writes the chosen state back into the config and the backend payload', async () => {
    const { curNetwork, wrapper } = mountConfig()
    await nextTick()

    await options(wrapper)[1].trigger('click')
    await nextTick()

    expect(curNetwork.direct_conn_family).toBe('prefer_ipv4')
    expect(selected(wrapper)).toBe('prefer_ipv4')
    expect(toBackendNetworkConfig(curNetwork).direct_conn_family).toBe('prefer_ipv4')

    await options(wrapper)[2].trigger('click')
    await nextTick()

    expect(curNetwork.direct_conn_family).toBe('prefer_ipv6')
    expect(toBackendNetworkConfig(curNetwork).direct_conn_family).toBe('prefer_ipv6')
  })

  it('states auto explicitly once it is chosen', async () => {
    const config = DEFAULT_NETWORK_CONFIG()
    config.direct_conn_family = 'prefer_ipv4'
    const { curNetwork, wrapper } = mountConfig(config)
    await nextTick()

    await options(wrapper)[0].trigger('click')
    await nextTick()

    expect(curNetwork.direct_conn_family).toBe('auto')
    expect(toBackendNetworkConfig(curNetwork).direct_conn_family).toBe('auto')
  })

  it('translates the label and the help of the selector', () => {
    expect(cn[FIELD]).toBeTruthy()
    expect(cn[`${FIELD}_help`]).toBeTruthy()
    expect(en[FIELD]).toBeTruthy()
    expect(en[`${FIELD}_help`]).toBeTruthy()
  })
})
