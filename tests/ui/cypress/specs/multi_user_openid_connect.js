// The mock OpenID Connect provider only checks usernames, not passwords.
let admin        = { u: 'admin@krill' };
let readonly     = { u: 'readonly@krill' };
let readwrite    = { u: 'readwrite@krill' };
let shorttoken   = { u: 'shorttokenwithoutrefresh@krill' };
let shortrefresh = { u: 'shorttokenwithrefresh@krill' };
let badidtoken   = { u: 'non-spec-compliant-idtoken-payload' };
let badrole      = { u: 'user-with-unknown-role' };
let ca_name      = 'dummy-ca-name';

let login_test_settings = [
  { d: 'empty',        u: '',                o: false },
  { d: 'incorrect',    u: 'wrong_user_name', o: false },
  { d: 'admin',        u: admin.u,           o: true  },
  { d: 'readonly',     u: readonly.u,        o: true  },
  { d: 'readwrite',    u: readwrite.u,       o: true },
  { d: 'badidtoken',   u: badidtoken.u,      o: false },
  { d: 'badrole',      u: badrole.u,         o: false }
];

describe('OpenID Connect users', () => {
  it('The correct login form is shown', () => {
    cy.intercept('GET', '/api/v1/authorized').as('isAuthorized')
    cy.intercept('GET', '/auth/login').as('getLoginURL')
    cy.intercept('GET', /^http:\/\/localhost:1818\/authorize.+/).as('oidcLoginForm')
    cy.visit('/')
    cy.wait(['@isAuthorized', '@getLoginURL', '@oidcLoginForm'])

    // make sure we haven't been redirected away from Krill (as would be the
    // case if an OpenID Connect login form were shown)
    cy.url().should('not.include', Cypress.config('baseUrl'))

    // make sure that this is our mock OpenID Connect provider
    cy.contains('Mock OpenID Connect login form')

    // check that a username input field is shown on the page
    cy.get('input[name="username"]')
  })

  login_test_settings.forEach(function (ts) {
    it('Login with ' + ts.d + ' credentials should ' + (ts.o ? 'succeed with the expected user info' : 'fail with the expected error'), () => {
      cy.visit('/')
      cy.url().should('not.include', Cypress.config('baseUrl'))
      cy.contains('Mock OpenID Connect login form')

      if (ts.u != '') cy.get('input[name="username"]').clear().type(ts.u)

      cy.contains('Sign In').click()

      // We should end up back in the Krill UI
      cy.url().should('include', Cypress.config('baseUrl'))

      if (ts.o) {
        cy.contains('Sign In').should('not.exist')
        cy.get('#userinfo').click()
        cy.get('#userinfo_table').contains(ts.u)
        cy.get('#userinfo_table').contains("role")
      } else if (ts.d == 'empty') {
        cy.contains('The supplied login credentials were incorrect')
        cy.contains('return to the login page')
      } else if (ts.d == 'badidtoken') {
        cy.contains('OpenID Connect: Code exchange failed: Failed to parse server response')
        cy.contains('return to the login page')
      } else if (ts.d == 'badrole') {
        cy.contains('Your user does not have sufficient rights to perform this action. Please contact your administrator.')
        cy.contains('return to the login page')
      }
    })
  })

  it('Can logout', () => {
    // login
    cy.visit('/')
    cy.url().should('not.include', Cypress.config('baseUrl'))
    cy.contains('Mock OpenID Connect login form')
    cy.get('input[name="username"]').clear().type(admin.u)
    cy.contains('Sign In').click()

    // verify that we are shown to be logged in to the Krill UI
    cy.contains('Sign In').should('not.exist')
    cy.url().should('include', Cypress.config('baseUrl'))
    cy.get('#userinfo').click()
    cy.get('#userinfo_table').contains(admin.u)

    // logout
    cy.intercept('GET', /^http:\/\/localhost:1818\/logout.+/).as('oidcLogout')
    cy.get('.logout').click()
    cy.wait('@oidcLogout').its('response.statusCode').should('eq', 302)

    // verify that we are shown the OpenID Connect provider login page
    cy.url().should('not.include', Cypress.config('baseUrl'))
    cy.contains('Mock OpenID Connect login form')
    cy.get('input[name="username"]')
  })

  it('Login receives short-lived token that cannot be refreshed', () => {
    cy.intercept('GET', '/api/v1/authorized').as('isAuthorized')
    cy.visit('/')

    cy.wait('@isAuthorized').its('response.statusCode').should('eq', 403)
    cy.url().should('not.include', Cypress.config('baseUrl'))
    cy.contains('Mock OpenID Connect login form')
    cy.get('input[name="username"]').clear().type(shorttoken.u)

    cy.intercept('GET', '/index.html').as('postLoginIndexFetch')
    cy.intercept('GET', '/api/v1/authorized').as('isAuthorized')
    cy.contains('Sign In').click()

    cy.wait('@postLoginIndexFetch').its('response.statusCode').should('eq', 200)
    cy.wait('@isAuthorized').its('response.statusCode').should('eq', 200)
    cy.url().should('include', Cypress.config('baseUrl'))
    cy.contains('Sign In').should('not.exist')
    cy.get('#userinfo').click()
    cy.get('#userinfo_table').contains(shorttoken.u)
    cy.contains(shorttoken.u)
    cy.contains('Welcome to Krill')

    // the token has a lifetime of 5 second and no refresh token
    // wait 6 seconds...
    // note: a shorter token with a 1 second lifetime doesn't work in the GitHub
    // Action runner environment because the token has sometimes already expired
    // by the time Krill verifies it!
    cy.wait(6000)

    // verify that we are shown the OpenID Connect provider login page
    // cy.intercept('GET', '/api/v1/authorized').as('isAuthorized')
    cy.intercept('GET', '/auth/login').as('getLoginURL')
    cy.intercept('GET', /^http:\/\/localhost:1818\/authorize.+/).as('oidcLoginForm')

    cy.visit('/')
    // not sure why but even though the 401 response is sent and Cypress debug
    // logs show it, the following test never finds a match...
    // cy.wait('@isAuthorized').its('response.statusCode').should('eq', 401)
    cy.wait(['@getLoginURL', '@oidcLoginForm'])

    cy.url().should('not.include', Cypress.config('baseUrl'))
    cy.contains('Mock OpenID Connect login form')
    cy.get('input[name="username"]')
  })

  it('Login receives short-lived refreshable token', () => {
    cy.visit('/')
    cy.url().should('not.include', Cypress.config('baseUrl'))
    cy.contains('Mock OpenID Connect login form')
    cy.get('input[name="username"]').type(shortrefresh.u)
    cy.contains('Sign In').click()

    cy.url().should('include', Cypress.config('baseUrl'))
    cy.contains('Sign In').should('not.exist')
    cy.get('#userinfo').click()
    cy.get('#userinfo_table').contains(shortrefresh.u)
    cy.contains(shortrefresh.u)
    cy.contains('Welcome to Krill')

    for (let i = 0; i < 5; i++) {
      // the token has a lifetime of 5 seconds and has a refresh token
      // wait 6 seconds..
      // note: a shorter token with a 1 second lifetime doesn't work in the
      // GitHub Action runner environment because the token has sometimes
      // already expired by the time Krill verifies it!
      cy.wait(6000)

      // verify that we are still logged in to Krill
      cy.visit('/')
      cy.url().should('include', Cypress.config('baseUrl'))
      cy.contains('Sign In').should('not.exist')
      cy.get('#userinfo').click()
      cy.get('#userinfo_table').contains(shortrefresh.u)
      cy.contains(shortrefresh.u)
      cy.contains('Welcome to Krill')
    }
  })
})