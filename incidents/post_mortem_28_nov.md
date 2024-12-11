# Post-Mortem: High Missed Block Rate Incident on 28/11/2024

## Summary
On 28th of November, our relay experienced an incident that caused a high missed block rate due to a builder sending invalid blocks, which our relay processed optimistically. During this period, the simulator was under heavy load and became unresponsive. Requests to the simulator started to timeout, and the relay only demoted builders if the block validation failed, not if the simulation request timed out, so the blocks continued to be processed optimistically. Headers were served to validators for invalid blocks, leading to missed slots.

This incident has since been resolved, and all affected users have been refunded for missed slots where invalid headers were served by our relay. A patch addressing the relay demotion failure has been deployed: builders will now be demoted if anything other than a successful response is received from the simulator. Additionally, we are implementing safeguards to prevent similar incidents in the future.

We sincerely apologise for the disruption caused and are committed to ensuring the integrity and reliability of our relay operations.

---

## Timeline

| **Time (UTC)** | **Event**                                                                 |
|-----------------|---------------------------------------------------------------------------|
| **05:52**       | Terrence flagged an unusually high missed block rate in the CL + Relay Security group. |
| **06:39**       | Report received from Lido about high missed block rate.                  |
| **07:01**       | An engineer (outside the builder/relay team) identified a potential issue. |
| **07:39**       | Builder was shutdown.                               |
| **07:54**       | Relay stopped serving headers.                                          |
| **11:58**       | All missed slots and relays that delivered the payloads identified.      |
| **15:56**       | Refunds issued to everyone that missed slots where Titan relay served an invalid header. |
| **16:09**       | Issue with relay demotion logic patched and deployed.                    |

---

## Root Cause

### Builder Sending Invalid blocks
- A builder sent invalid blocks to our relay.
- Our relay attempted to propagate these, leading to missed slots.

### Relay Demotion Failure
- During this incident, the relay simulator was under heavy load and became unresponsive.
- This caused timeouts, but the relay demotion mechanism failed to trigger a demotion for the timeout.

---

## Impacted Slots

| slot_number | timestamp          | builder | relays                                   | value        |
|-------------|---------------------|---------|------------------------------------------|--------------|
| 10495509    | 2024-11-28 05:02:11 | Titan   | {BLOXROUTE_REGULATED}                   | 0.1012995812 |
| 10495511    | 2024-11-28 05:02:35 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.08670800005 |
| 10495516    | 2024-11-28 05:03:35 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.07679655259 |
| 10495523    | 2024-11-28 05:04:59 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.01936939929 |
| 10495525    | 2024-11-28 05:05:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.1007419941 |
| 10495530    | 2024-11-28 05:06:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.1762723579 |
| 10495540    | 2024-11-28 05:08:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.1983884411 |
| 10495546    | 2024-11-28 05:09:35 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.04562308907 |
| 10495563    | 2024-11-28 05:12:59 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.02807875085 |
| 10495583    | 2024-11-28 05:16:59 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.03180092671 |
| 10495586    | 2024-11-28 05:17:35 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.03638768282 |
| 10495605    | 2024-11-28 05:21:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.05797291933 |
| 10495607    | 2024-11-28 05:21:47 | Titan   | {BLOXROUTE_MAXPROFIT}                   | 0.07330244084 |
| 10495610    | 2024-11-28 05:22:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.1035902021 |
| 10495612    | 2024-11-28 05:22:47 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.1311046699 |
| 10495615    | 2024-11-28 05:23:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.03895845128 |
| 10495620    | 2024-11-28 05:24:23 | Titan   | {BLOXROUTE_MAXPROFIT}                   | 0.09395324582 |
| 10495647    | 2024-11-28 05:29:47 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.02883464859 |
| 10495657    | 2024-11-28 05:31:47 | Titan   | {BLOXROUTE_MAXPROFIT}                   | 0.06311835209 |
| 10495661    | 2024-11-28 05:32:35 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.0430433011 |
| 10495665    | 2024-11-28 05:33:23 | Titan   | {BLOXROUTE_MAXPROFIT}                   | 0.03701124456 |
| 10495670    | 2024-11-28 05:34:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.02177618593 |
| 10495679    | 2024-11-28 05:36:11 | Titan   | {BLOXROUTE_MAXPROFIT}                   | 0.03264169595 |
| 10495682    | 2024-11-28 05:36:47 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.08233741471 |
| 10495718    | 2024-11-28 05:43:59 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.01868150591 |
| 10495749    | 2024-11-28 05:50:11 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.03788347747 |
| 10495752    | 2024-11-28 05:50:47 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED} | 0.02622301218 |
| 10495762    | 2024-11-28 05:52:47 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.02870457253 |
| 10495775    | 2024-11-28 05:55:23 | Titan   | {BLOXROUTE_REGULATED,TITAN}             | 0.0170804317  |
| 10495782    | 2024-11-28 05:56:47 | Titan   | {TITAN}                                 | 0.01840114176 |
| 10495839    | 2024-11-28 06:08:11 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.02638451642 |
| 10495891    | 2024-11-28 06:18:35 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.0188595145  |
| 10495908    | 2024-11-28 06:21:59 | Titan   | {BLOXROUTE_REGULATED,TITAN}             | 0.03087671051 |
| 10495930    | 2024-11-28 06:26:23 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.02375264723 |
| 10495968    | 2024-11-28 06:33:59 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.03277077322 |
| 10495996    | 2024-11-28 06:39:35 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.01382764676 |
| 10495999    | 2024-11-28 06:40:11 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.07843133485 |
| 10496004    | 2024-11-28 06:41:11 | Titan   | {BLOXROUTE_REGULATED,TITAN}             | 0.03081040632 |
| 10496006    | 2024-11-28 06:41:35 | Titan   | {TITAN}                                 | 0.02125548031 |
| 10496021    | 2024-11-28 06:44:35 | Titan   | {BLOXROUTE_MAXPROFIT,BLOXROUTE_REGULATED,TITAN} | 0.0240545745  |
| 10496065    | 2024-11-28 06:53:23 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.02858363319 |
| 10496117    | 2024-11-28 07:03:47 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.01151285008 |
| 10496133    | 2024-11-28 07:06:59 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.04629570544 |
| 10496141    | 2024-11-28 07:08:35 | Titan   | {TITAN}                                 | 0.1058638799  |
| 10496174    | 2024-11-28 07:15:11 | Titan   | {TITAN}                                 | 0.01780644328 |
| 10496230    | 2024-11-28 07:26:23 | Titan   | {BLOXROUTE_MAXPROFIT,TITAN}             | 0.02206454917 |

Note: Bloxroute disabled headers at slot 10495752. However, their logic continues to serve get_payload requests, marking the payload as delivered while withholding headers for invalid blocks, which is why you still see Bloxroute relays in the CSV after this slot.

For a full detailed report of impacted slots, [download the CSV file](./invalid_merkle_root_incident_impact.csv).




## Mitigations

### Builder
- Builder was shut down at **07:39 UTC**.

### Relay Demotion Patch
- The demotion mechanism in the relay has been patched to ensure demotion triggers correctly when simulation times out.
  - [View Patch](https://github.com/gattaca-com/helix/pull/51/files)

### User Refunds
- Affected proposers were refunded for missed slots where Titan relay served invalid headers.
  - [View Refund Transaction](https://etherscan.io/tx/0xc8811bb7d51d4bd66e51a6d88d23992f5a74fc2c8e8eff4641d26f8ac5b82149)

---

## Looking Forward

We have identified several key areas to improve resilience and reliability in our relay operations:

1. **Simulator Load Handling**
   - Conducting stress tests and optimizations to ensure the simulator remains responsive under heavy loads.

2. **Incident Response Enhancements**
   - Refining incident response processes to improve communication and escalation paths for faster resolution.

3. **Demotion Enhancements**
   - We are looking into even more robust builder demotion mechanisms including faster demotions but also collaborative builder demotion gossiping across different relay operators.
   
4. **Bug bounty**
   - We will establish a bug bounty program for our open source relay code. While our relay has been audited, bugs like this demotion failure can still be found, highlighting the importance a community driven open source approach. We invite contributions from the community to continue improving the reliability and security of our relay infrastructure. If you have suggestions or would like to participate, please reach out.
