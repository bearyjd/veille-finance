# Phase 0 seed: two families with distinct users and distinctive canary data.
# Run inside the web container: bin/rails runner /path/to/seed_two_families.rb

# --- Family A: full realistic demo dataset, admin = alpha.owner@phase0.test ---
gen = Demo::Generator.new(seed: 42)
gen.generate_default_data!(skip_clear: true, email: "alpha.owner@phase0.test")

# --- Family B: minimal family with unmistakably named canary data ---
family_b = Family.create!(
  name: "Beta Family",
  currency: "USD", locale: "en", country: "US",
  timezone: "America/New_York", date_format: "%m-%d-%Y"
)
user_b = family_b.users.create!(
  email: "beta.owner@phase0.test", first_name: "Beta", last_name: "Owner",
  role: "admin", password: "Password1!", onboarded_at: Time.current
)
acct_b = family_b.accounts.create!(
  accountable: Depository.new(subtype: "checking"),
  name: "BETA-CANARY-CHECKING", balance: 4242.42, currency: "USD",
  owner: user_b
)
acct_b.entries.create!(
  entryable: Transaction.new,
  amount: -1234.56,
  name: "BETA-CANARY-DEPOSIT",
  currency: "USD",
  date: Date.current - 3
)

# --- Report state relevant to the scoping experiment ---
ua = User.find_by(email: "alpha.owner@phase0.test")
fa = ua.family
puts "RESULT family_a_id=#{fa.id} name=#{fa.name}"
puts "RESULT family_b_id=#{family_b.id} name=#{family_b.name}"
puts "RESULT users=#{User.order(:email).pluck(:email, :role).inspect}"
puts "RESULT family_a_accounts=#{fa.accounts.count} owners=#{fa.accounts.pluck(:owner_id).uniq.inspect}"
puts "RESULT alpha_accessible=#{ua.accessible_accounts.count} of #{fa.accounts.count}"
puts "RESULT beta_accounts=#{family_b.accounts.pluck(:name).inspect}"
