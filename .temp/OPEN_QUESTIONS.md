let bid: f64 = entry.best_bid.as_ref().map(|v| v.to_string().parse().unwrap_or(0.0)).unwrap_or(0.0);
                                let ask: f64 = entry.best_ask.as_ref().map(|v| v.to_string().parse().unwrap_or(0.0)).unwrap_or(0.0);

                                if bid <= 0.0 || ask <= 0.0 {
                                    continue;
                                }

                                let is_up = entry.asset_id == m.up.token_id;
                                let is_down = entry.asset_id == m.down.token_id;
                                if !is_up && !is_down {
                                    continue;
                                }

                                // Validate: up mid + down mid should be ~1.0
                                let (new_up_mid, new_down_mid) = if is_up {
                                    ((bid + ask) / 2.0, (m.down.best_bid + m.down.best_ask) / 2.0)
                                } else {
                                    ((m.up.best_bid + m.up.best_ask) / 2.0, (bid + ask) / 2.0)
                                };
                                let sum = new_up_mid + new_down_mid;
                                // Skip validation if the other side hasn't been set yet
                                if new_down_mid > 0.0 && new_up_mid > 0.0 && (sum < 0.9 || sum > 1.1) {
                                    continue;
                                }

if strike_chainlink == 1900.0 {
                    let wait_secs = (info.expires_ms / 1000).saturating_sub(now_secs()).max(1) as u64;
                    warn!("No strike price for {}. Skipping market.", info.slug);
                    sleep(Duration::from_secs(wait_secs)).await;
                    continue;
                }