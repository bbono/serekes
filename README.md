In production
nohup cargo run --release > bot.log 2>&1 &                                                                                                                                                                                                                                 
                                                                                                                                                                                                                                                                             
  Then you can close the terminal. Check logs with tail -f bot.log.                                                                                                                                                                                                          
                                                            
  To stop it later: kill $(pgrep serekes)